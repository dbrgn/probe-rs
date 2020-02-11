use super::builder::FlashBuilder;
use super::FlashProgress;
use crate::config::{FlashAlgorithm, FlashRegion, MemoryRange, SectorInfo};
use crate::core::{Core, RegisterFile};
use crate::error;
use crate::memory::MemoryInterface;
use crate::session::Session;
use thiserror::Error;

pub trait Operation {
    fn operation() -> u32;
    fn operation_name(&self) -> &str {
        match Self::operation() {
            1 => "Erase",
            2 => "Program",
            3 => "Verify",
            _ => "Unknown Operation",
        }
    }
}

pub struct Erase;

impl Operation for Erase {
    fn operation() -> u32 {
        1
    }
}

pub struct Program;

impl Operation for Program {
    fn operation() -> u32 {
        2
    }
}

pub struct Verify;

impl Operation for Verify {
    fn operation() -> u32 {
        3
    }
}

#[derive(Error, Debug)]
pub enum FlasherError {
    #[error("The execution of '{name}' failed with code {errorcode}")]
    RoutineCallFailed { name: &'static str, errorcode: u32 },
    #[error("'{0}' is not supported")]
    NotSupported(&'static str),
    #[error("Buffer {n}/{max} does not exist")]
    InvalidBufferNumber { n: usize, max: usize },
    #[error("Something during memory interaction went wrong")]
    Memory(#[source] error::Error),
    #[error("Something during memory interaction went wrong")]
    Core(#[source] error::Error),
    #[error("{address} is not contained in {region:?}")]
    AddressNotInRegion { address: u32, region: FlashRegion },
}

pub struct Flasher<'a> {
    session: Session,
    flash_algorithm: &'a FlashAlgorithm,
    region: &'a FlashRegion,
    double_buffering_supported: bool,
}

impl<'a> Flasher<'a> {
    pub fn new(
        session: Session,
        flash_algorithm: &'a FlashAlgorithm,
        region: &'a FlashRegion,
    ) -> Self {
        Self {
            session,
            flash_algorithm,
            region,
            double_buffering_supported: false,
        }
    }

    pub fn region(&self) -> &FlashRegion {
        &self.region
    }

    /// Returns the necessary information about the sector which `address` resides in
    /// if the address is inside the flash region.
    pub fn sector_info(&self, address: u32) -> Option<SectorInfo> {
        self.flash_algorithm.sector_info(address)
    }

    pub fn flash_algorithm(&self) -> &FlashAlgorithm {
        &self.flash_algorithm
    }

    pub fn double_buffering_supported(&self) -> bool {
        self.double_buffering_supported
    }

    pub fn init<'b, 's: 'b, O: Operation>(
        &'s mut self,
        mut address: Option<u32>,
        clock: Option<u32>,
    ) -> Result<ActiveFlasher<'b, O>, FlasherError> {
        log::debug!("Initializing the flash algorithm.");
        let flasher = self;
        let algo = flasher.flash_algorithm;

        use capstone::arch::*;
        let cs = capstone::Capstone::new()
            .arm()
            .mode(arm::ArchMode::Thumb)
            .endian(capstone::Endian::Little)
            .build()
            .unwrap();
        let i = algo
            .instructions
            .iter()
            .map(|i| {
                [
                    *i as u8,
                    (*i >> 8) as u8,
                    (*i >> 16) as u8,
                    (*i >> 24) as u8,
                ]
            })
            .collect::<Vec<[u8; 4]>>()
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<u8>>();

        let instructions = cs
            .disasm_all(i.as_slice(), u64::from(algo.load_address))
            .unwrap();

        for instruction in instructions.iter() {
            log::trace!("{}", instruction);
        }

        if address.is_none() {
            address = Some(flasher.region.flash_info().rom_start);
        }

        // Attach to memory and core.
        let mut core = flasher
            .session
            .attach_to_core(0)
            .map_err(FlasherError::Memory)?;

        // TODO: Halt & reset target.
        log::debug!("Halting core.");
        let cpu_info = core.halt();
        log::debug!("PC = 0x{:08x}", cpu_info.unwrap().pc);
        core.wait_for_core_halted().map_err(FlasherError::Core)?;
        log::debug!("Reset and halt");
        core.reset_and_halt().map_err(FlasherError::Core)?;

        // TODO: Possible special preparation of the target such as enabling faster clocks for the flash e.g.

        // Load flash algorithm code into target RAM.
        log::debug!(
            "Loading algorithm into RAM at address 0x{:08x}",
            algo.load_address
        );

        core.write_block32(algo.load_address, algo.instructions.as_slice())
            .map_err(FlasherError::Memory)?;

        let mut data = vec![0; algo.instructions.len()];
        core.read_block32(algo.load_address, &mut data)
            .map_err(FlasherError::Memory)?;

        for (offset, (original, read_back)) in algo.instructions.iter().zip(data.iter()).enumerate()
        {
            if original != read_back {
                eprintln!(
                    "Failed to verify flash algorithm. Data mismatch at address {:#08x}",
                    algo.load_address + (4 * offset) as u32
                );
                eprintln!("Original instruction: {:#08x}", original);
                eprintln!("Readback instruction: {:#08x}", read_back);

                eprintln!("Original: {:x?}", &algo.instructions);
                eprintln!("Readback: {:x?}", &data);

                panic!("Flash algorithm not written to flash correctly.");
            }
        }

        log::debug!("RAM contents match flashing algo blob.");

        log::debug!("Preparing Flasher for region:");
        log::debug!("{:#?}", &flasher.region);
        log::debug!(
            "Double buffering enabled: {}",
            flasher.double_buffering_supported
        );
        let mut flasher = ActiveFlasher {
            core: core,
            flash_algorithm: flasher.flash_algorithm,
            region: flasher.region,
            _double_buffering_supported: flasher.double_buffering_supported,
            _operation: core::marker::PhantomData,
        };

        flasher.init(address, clock)?;

        Ok(flasher)
    }

    pub fn run_erase<T, E: From<FlasherError>>(
        &mut self,
        f: impl FnOnce(&mut ActiveFlasher<Erase>) -> Result<T, E> + Sized,
    ) -> Result<T, E> {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None, None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub fn run_program<T, E: From<FlasherError>>(
        &mut self,
        f: impl FnOnce(&mut ActiveFlasher<Program>) -> Result<T, E> + Sized,
    ) -> Result<T, E> {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None, None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub fn run_verify<T, E: From<FlasherError>>(
        &mut self,
        f: impl FnOnce(&mut ActiveFlasher<Verify>) -> Result<T, E> + Sized,
    ) -> Result<T, E> {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None, None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub fn flash_block(
        self,
        address: u32,
        data: &[u8],
        progress: &FlashProgress,
        do_chip_erase: bool,
        _fast_verify: bool,
    ) -> Result<(), FlasherError> {
        if !self
            .region
            .range
            .contains_range(&(address..address + data.len() as u32))
        {
            return Err(FlasherError::AddressNotInRegion {
                address,
                region: self.region.clone(),
            });
        }

        let mut fb = FlashBuilder::new();
        fb.add_data(address, data).expect("Add Data failed");
        fb.program(self, do_chip_erase, true, progress)
            .expect("Add Data failed");

        Ok(())
    }
}

pub struct ActiveFlasher<'a, O: Operation> {
    core: Core,
    flash_algorithm: &'a FlashAlgorithm,
    region: &'a FlashRegion,
    _double_buffering_supported: bool,
    _operation: core::marker::PhantomData<O>,
}

impl<'a, O: Operation> ActiveFlasher<'a, O> {
    pub fn init(&mut self, address: Option<u32>, clock: Option<u32>) -> Result<(), FlasherError> {
        let algo = &self.flash_algorithm;
        log::debug!("Running init routine.");

        // Execute init routine if one is present.
        if let Some(pc_init) = algo.pc_init {
            let result = self.call_function_and_wait(
                pc_init,
                address,
                clock.or(Some(0)),
                Some(O::operation()),
                None,
                true,
            )?;

            if result != 0 {
                return Err(FlasherError::RoutineCallFailed {
                    name: "init",
                    errorcode: result,
                });
            }
        }

        Ok(())
    }

    pub fn region(&self) -> &FlashRegion {
        &self.region
    }

    pub fn flash_algorithm(&self) -> &FlashAlgorithm {
        &&self.flash_algorithm
    }

    // pub fn session_mut(&mut self) -> &mut Session {
    //     &mut self.session
    // }

    pub fn uninit<'b, 's: 'b>(&'s mut self) -> Result<(), FlasherError> {
        log::debug!("Running uninit routine.");
        let algo = &self.flash_algorithm;

        if let Some(pc_uninit) = algo.pc_uninit {
            let result = self.call_function_and_wait(
                pc_uninit,
                Some(O::operation()),
                None,
                None,
                None,
                false,
            )?;

            if result != 0 {
                return Err(FlasherError::RoutineCallFailed {
                    name: "uninit",
                    errorcode: result,
                });
            }
        }
        Ok(())
    }

    fn call_function_and_wait(
        &mut self,
        pc: u32,
        r0: Option<u32>,
        r1: Option<u32>,
        r2: Option<u32>,
        r3: Option<u32>,
        init: bool,
    ) -> Result<u32, FlasherError> {
        self.call_function(pc, r0, r1, r2, r3, init)?;
        self.wait_for_completion()
    }

    fn call_function(
        &mut self,
        pc: u32,
        r0: Option<u32>,
        r1: Option<u32>,
        r2: Option<u32>,
        r3: Option<u32>,
        init: bool,
    ) -> Result<(), FlasherError> {
        log::debug!(
            "Calling routine {:08x}({:?}, {:?}, {:?}, {:?}, init={})",
            pc,
            r0,
            r1,
            r2,
            r3,
            init
        );

        let algo = &self.flash_algorithm;
        let regs: &'static RegisterFile = self.core.registers();

        [
            (regs.program_counter(), Some(pc)),
            (regs.argument_register(0), r0),
            (regs.argument_register(1), r1),
            (regs.argument_register(2), r2),
            (regs.argument_register(3), r3),
            (
                regs.platform_register(9),
                if init { Some(algo.static_base) } else { None },
            ),
            (
                regs.stack_pointer(),
                if init { Some(algo.begin_stack) } else { None },
            ),
            (regs.return_address(), Some(algo.load_address + 1)),
        ]
        .iter()
        .map(|(description, value)| {
            if let Some(v) = value {
                self.core.write_core_reg(description.address, *v)?;
                log::debug!(
                    "content of {:#x}: 0x{:08x} should be: 0x{:08x}",
                    description.address.0,
                    self.core.read_core_reg(description.address)?,
                    *v
                );
                Ok(())
            } else {
                Ok(())
            }
        })
        .collect::<Result<Vec<()>, error::Error>>()
        .map_err(FlasherError::Core)?;

        // Resume target operation.
        self.core.run().map_err(FlasherError::Core)?;

        Ok(())
    }

    pub fn wait_for_completion(&mut self) -> Result<u32, FlasherError> {
        log::debug!("Waiting for routine call completion.");
        let regs = self.core.registers();

        while self.core.wait_for_core_halted().is_err() {}

        let r = self
            .core
            .read_core_reg(regs.result_register(0).address)
            .map_err(FlasherError::Core)?;
        Ok(r)
    }

    pub fn read_block32(&mut self, address: u32, data: &mut [u32]) -> Result<(), FlasherError> {
        self.core
            .memory()
            .read_block32(address, data)
            .map_err(FlasherError::Memory)?;
        Ok(())
    }

    pub fn read_block8(&mut self, address: u32, data: &mut [u8]) -> Result<(), FlasherError> {
        self.core
            .memory()
            .read_block8(address, data)
            .map_err(FlasherError::Memory)?;
        Ok(())
    }
}

impl<'a> ActiveFlasher<'a, Erase> {
    pub fn erase_all(&mut self) -> Result<(), FlasherError> {
        log::debug!("Erasing entire chip.");
        let flasher = self;
        let algo = flasher.flash_algorithm;

        if let Some(pc_erase_all) = algo.pc_erase_all {
            let result =
                flasher.call_function_and_wait(pc_erase_all, None, None, None, None, false)?;

            if result != 0 {
                Err(FlasherError::RoutineCallFailed {
                    name: "erase_all",
                    errorcode: result,
                })
            } else {
                Ok(())
            }
        } else {
            Err(FlasherError::NotSupported("erase_all"))
        }
    }

    pub fn erase_sector(&mut self, address: u32) -> Result<(), FlasherError> {
        log::info!("Erasing sector at address 0x{:08x}", address);
        let t1 = std::time::Instant::now();
        let flasher = self;
        let algo = flasher.flash_algorithm;

        let result = flasher.call_function_and_wait(
            algo.pc_erase_sector,
            Some(address),
            None,
            None,
            None,
            false,
        )?;
        log::info!(
            "Done erasing sector. Result is {}. This took {:?}",
            result,
            t1.elapsed()
        );

        if result != 0 {
            Err(FlasherError::RoutineCallFailed {
                name: "erase_sector",
                errorcode: result,
            })
        } else {
            Ok(())
        }
    }
}

impl<'a> ActiveFlasher<'a, Program> {
    pub fn program_page(&mut self, address: u32, bytes: &[u8]) -> Result<(), FlasherError> {
        let t1 = std::time::Instant::now();
        let flasher = self;
        let algo = flasher.flash_algorithm;

        log::info!(
            "Flashing page at address {:#08x} with size: {}",
            address,
            bytes.len()
        );

        // Transfer the bytes to RAM.
        flasher
            .core
            .memory()
            .write_block8(algo.begin_data, bytes)
            .map_err(FlasherError::Memory)?;
        let result = flasher.call_function_and_wait(
            algo.pc_program_page,
            Some(address),
            Some(bytes.len() as u32),
            Some(algo.begin_data),
            None,
            false,
        )?;
        log::info!("Flashing took: {:?}", t1.elapsed());

        if result != 0 {
            Err(FlasherError::RoutineCallFailed {
                name: "program_page",
                errorcode: result,
            })
        } else {
            Ok(())
        }
    }

    pub fn start_program_page_with_buffer(
        &mut self,
        address: u32,
        buffer_number: usize,
    ) -> Result<(), FlasherError> {
        let flasher = self;
        let algo = flasher.flash_algorithm;

        // Check the buffer number.
        if buffer_number < algo.page_buffers.len() {
            return Err(FlasherError::InvalidBufferNumber {
                n: buffer_number,
                max: algo.page_buffers.len(),
            });
        }

        flasher.call_function(
            algo.pc_program_page,
            Some(address),
            Some(flasher.flash_algorithm().flash_properties.page_size),
            Some(algo.page_buffers[buffer_number as usize]),
            None,
            false,
        )?;

        Ok(())
    }

    pub fn load_page_buffer(
        &mut self,
        _address: u32,
        bytes: &[u8],
        buffer_number: usize,
    ) -> Result<(), FlasherError> {
        let flasher = self;
        let algo = flasher.flash_algorithm;

        // Check the buffer number.
        if buffer_number < algo.page_buffers.len() {
            return Err(FlasherError::InvalidBufferNumber {
                n: buffer_number,
                max: algo.page_buffers.len(),
            });
        }

        // TODO: Prevent security settings from locking the device.

        // Transfer the buffer bytes to RAM.
        flasher
            .core
            .memory()
            .write_block8(algo.page_buffers[buffer_number as usize], bytes)
            .map_err(FlasherError::Memory)?;

        Ok(())
    }
}

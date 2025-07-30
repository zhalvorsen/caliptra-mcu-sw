// Licensed under the Apache-2.0 license

#![allow(clippy::mut_from_ref)]

use crate::fpga_regs::{Control, FifoData, FifoRegs, FifoStatus, ItrngFifoStatus, WrapperRegs};
use crate::output::ExitStatus;
use crate::{xi3c, InitParams, McuHwModel, Output, SecurityState};
use anyhow::{anyhow, bail, Error, Result};
use caliptra_api::SocManager;
use caliptra_emu_bus::{Bus, BusError, BusMmio, Device, Event, EventData, RecoveryCommandCode};
use caliptra_emu_types::{RvAddr, RvData, RvSize};
use caliptra_hw_model_types::{DEFAULT_FIELD_ENTROPY, DEFAULT_UDS_SEED};
use emulator_bmc::Bmc;
use registers_generated::i3c;
use registers_generated::i3c::bits::{DeviceStatus0, StbyCrDeviceAddr, StbyCrVirtDeviceAddr};
use registers_generated::mci::bits::Go::Go;
use std::io::Write;
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use tock_registers::interfaces::{ReadWriteable, Readable, Writeable};
use uio::{UioDevice, UioError};

// UIO mapping indices
const FPGA_WRAPPER_MAPPING: (usize, usize) = (0, 0);
const CALIPTRA_MAPPING: (usize, usize) = (0, 1);
const CALIPTRA_ROM_MAPPING: (usize, usize) = (0, 2);
const I3C_CONTROLLER_MAPPING: (usize, usize) = (0, 3);
const OTP_RAM_MAPPING: (usize, usize) = (0, 4);
const LC_MAPPING: (usize, usize) = (1, 0);
const MCU_ROM_MAPPING: (usize, usize) = (1, 1);
const I3C_TARGET_MAPPING: (usize, usize) = (1, 2);
const MCI_MAPPING: (usize, usize) = (1, 3);
const OTP_MAPPING: (usize, usize) = (1, 4);

// Set to core_clk cycles per ITRNG sample.
const ITRNG_DIVISOR: u32 = 400;
const DEFAULT_AXI_PAUSER: u32 = 0xcccc_cccc;
const OTP_SIZE: usize = 16384;

// ITRNG FIFO stores 1024 DW and outputs 4 bits at a time to Caliptra.
const FPGA_ITRNG_FIFO_SIZE: usize = 1024;

fn fmt_uio_error(err: UioError) -> Error {
    anyhow!("{err:?}")
}

struct Wrapper {
    ptr: *mut u32,
}

impl Wrapper {
    fn regs(&self) -> &mut WrapperRegs {
        unsafe { &mut *(self.ptr as *mut WrapperRegs) }
    }
    fn fifo_regs(&self) -> &mut FifoRegs {
        unsafe { &mut *(self.ptr.offset(0x1000 / 4) as *mut FifoRegs) }
    }
}
unsafe impl Send for Wrapper {}
unsafe impl Sync for Wrapper {}

struct Mci {
    ptr: *mut u32,
}

impl Mci {
    fn regs(&self) -> &mut registers_generated::mci::regs::Mci {
        unsafe { &mut *(self.ptr as *mut registers_generated::mci::regs::Mci) }
    }
}

struct CaliptraMmio {
    ptr: *mut u32,
}

impl CaliptraMmio {
    #[allow(unused)]
    fn mbox(&self) -> &mut registers_generated::mbox::regs::Mbox {
        unsafe {
            &mut *(self.ptr.offset(0x2_0000 / 4) as *mut registers_generated::mbox::regs::Mbox)
        }
    }
    #[allow(unused)]
    fn soc(&self) -> &mut registers_generated::soc::regs::Soc {
        unsafe { &mut *(self.ptr.offset(0x3_0000 / 4) as *mut registers_generated::soc::regs::Soc) }
    }
}

pub struct ModelFpgaRealtime {
    devs: [UioDevice; 2],
    // mmio uio pointers
    wrapper: Arc<Wrapper>,
    caliptra_mmio: CaliptraMmio,
    caliptra_rom_backdoor: *mut u8,
    mcu_rom_backdoor: *mut u8,
    otp_mem_backdoor: *mut u8,
    otp_init: Vec<u8>,
    lifecycle_controller_state: Option<u32>,
    mci: Mci,
    i3c_mmio: *mut u32,
    i3c_controller_mmio: *mut u32,
    i3c_controller: xi3c::Controller,

    realtime_thread: Option<thread::JoinHandle<()>>,
    realtime_thread_exit_flag: Arc<AtomicBool>,

    output: Output,
    recovery_started: bool,
    bmc: Bmc,
    from_bmc: mpsc::Receiver<Event>,
    to_bmc: mpsc::Sender<Event>,
    recovery_fifo_blocks: Vec<Vec<u8>>,
    recovery_ctrl_len: usize,
    recovery_ctrl_written: bool,
    bmc_step_counter: usize,
    i3c_target: &'static i3c::regs::I3c,
    blocks_sent: usize,
    openocd: Option<TcpStream>,
}

impl ModelFpgaRealtime {
    fn set_bootfsm_break(&mut self, val: bool) {
        if val {
            self.wrapper
                .regs()
                .control
                .modify(Control::BootfsmBrkpoint::SET);
        } else {
            self.wrapper
                .regs()
                .control
                .modify(Control::BootfsmBrkpoint::CLEAR);
        }
    }
    fn set_subsystem_reset(&mut self, reset: bool) {
        self.wrapper.regs().control.modify(
            Control::CptraSsRstB.val((!reset) as u32) + Control::CptraPwrgood.val((!reset) as u32),
        );
    }

    fn set_secrets_valid(&mut self, value: bool) {
        self.wrapper.regs().control.modify(
            Control::CptraObfUdsSeedVld.val(value as u32)
                + Control::CptraObfFieldEntropyVld.val(value as u32),
        )
    }

    fn clear_logs(&mut self) {
        println!("Clearing Caliptra logs");
        loop {
            if self
                .wrapper
                .fifo_regs()
                .log_fifo_status
                .is_set(FifoStatus::Empty)
            {
                break;
            }
            if !self
                .wrapper
                .fifo_regs()
                .log_fifo_data
                .is_set(FifoData::CharValid)
            {
                break;
            }
        }

        println!("Clearing MCU logs");
        loop {
            if self
                .wrapper
                .fifo_regs()
                .dbg_fifo_status
                .is_set(FifoStatus::Empty)
            {
                break;
            }
            if !self
                .wrapper
                .fifo_regs()
                .dbg_fifo_data_pop
                .is_set(FifoData::CharValid)
            {
                break;
            }
        }
    }

    fn handle_log(&mut self) {
        loop {
            // Check if the FIFO is full (which probably means there was an overrun)
            if self
                .wrapper
                .fifo_regs()
                .log_fifo_status
                .is_set(FifoStatus::Full)
            {
                panic!("FPGA log FIFO overran");
            }
            if self
                .wrapper
                .fifo_regs()
                .log_fifo_status
                .is_set(FifoStatus::Empty)
            {
                break;
            }
            let data = self.wrapper.fifo_regs().log_fifo_data.extract();
            // Add byte to log if it is valid
            if data.is_set(FifoData::CharValid) {
                self.output()
                    .sink()
                    .push_uart_char(data.read(FifoData::NextChar) as u8);
            }
        }

        loop {
            // Check if the FIFO is full (which probably means there was an overrun)
            if self
                .wrapper
                .fifo_regs()
                .dbg_fifo_status
                .is_set(FifoStatus::Full)
            {
                panic!("FPGA log FIFO overran");
            }
            if self
                .wrapper
                .fifo_regs()
                .dbg_fifo_status
                .is_set(FifoStatus::Empty)
            {
                break;
            }
            let data = self.wrapper.fifo_regs().dbg_fifo_data_pop.extract();
            // Add byte to log if it is valid
            if data.is_set(FifoData::CharValid) {
                self.output()
                    .sink()
                    .push_uart_char(data.read(FifoData::NextChar) as u8);
            }
        }
        if self.output().exit_requested() {
            println!("Exiting firmware request");
            let code = match self.output().exit_status() {
                Some(ExitStatus::Passed) => 0,
                Some(ExitStatus::Failed) => 1,
                None => 0,
            };
            exit(code);
        }
    }

    // UIO crate doesn't provide a way to unmap memory.
    fn unmap_mapping(&self, addr: *mut u32, mapping: (usize, usize)) {
        let map_size = self.devs[mapping.0].map_size(mapping.1).unwrap();

        unsafe {
            nix::sys::mman::munmap(addr as *mut libc::c_void, map_size).unwrap();
        }
    }

    fn realtime_thread_itrng_fn(
        wrapper: Arc<Wrapper>,
        running: Arc<AtomicBool>,
        mut itrng_nibbles: Box<dyn Iterator<Item = u8> + Send>,
    ) {
        // Reset ITRNG FIFO to clear out old data

        wrapper
            .fifo_regs()
            .itrng_fifo_status
            .write(ItrngFifoStatus::Reset::SET);
        wrapper
            .fifo_regs()
            .itrng_fifo_status
            .write(ItrngFifoStatus::Reset::CLEAR);

        // Small delay to allow reset to complete
        thread::sleep(Duration::from_millis(1));

        while running.load(Ordering::Relaxed) {
            // Once TRNG data is requested the FIFO will continously empty. Load at max one FIFO load at a time.
            // FPGA ITRNG FIFO is 1024 DW deep.
            for _ in 0..FPGA_ITRNG_FIFO_SIZE {
                if !wrapper
                    .fifo_regs()
                    .itrng_fifo_status
                    .is_set(ItrngFifoStatus::Full)
                {
                    let mut itrng_dw = 0;
                    for i in 0..8 {
                        match itrng_nibbles.next() {
                            Some(nibble) => itrng_dw += u32::from(nibble) << (4 * i),
                            None => return,
                        }
                    }
                    wrapper.fifo_regs().itrng_fifo_data.set(itrng_dw);
                } else {
                    break;
                }
            }
            // 1 second * (20 MHz / (2^13 throttling counter)) / 8 nibbles per DW: 305 DW of data consumed in 1 second.
            let end_time = Instant::now() + Duration::from_millis(1000);
            while running.load(Ordering::Relaxed) && Instant::now() < end_time {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    pub fn i3c_core(&mut self) -> &i3c::regs::I3c {
        unsafe { &*(self.i3c_mmio as *const i3c::regs::I3c) }
    }

    pub fn i3c_target_configured(&mut self) -> bool {
        let i3c_target = unsafe { &*(self.i3c_mmio as *const i3c::regs::I3c) };
        i3c_target.stdby_ctrl_mode_stby_cr_device_addr.get() != 0
    }

    pub fn configure_i3c_controller(&mut self) {
        println!("I3C controller initializing");
        println!(
            "XI3C HW version = {:x}",
            self.i3c_controller.regs().version.get()
        );
        let xi3c_config = xi3c::Config {
            device_id: 0,
            base_address: self.i3c_controller_mmio,
            input_clock_hz: 199_999_000,
            rw_fifo_depth: 16,
            wr_threshold: 12,
            device_count: 1,
            ibi_capable: true,
            hj_capable: false,
            entdaa_enable: true,
            known_static_addrs: vec![0x3a, 0x3b],
        };

        self.i3c_controller.set_s_clk(199_999_000, 12_500_000, 1);
        self.i3c_controller
            .cfg_initialize(&xi3c_config, self.i3c_controller_mmio as usize)
            .unwrap();
        println!("I3C controller finished initializing");
    }

    pub fn start_recovery_bmc(&mut self) {
        self.recovery_started = true;
    }

    fn bmc_step(&mut self) {
        if !self.recovery_started {
            return;
        }

        self.bmc_step_counter += 1;

        // check if we need to fill the recovey FIFO
        if self.bmc_step_counter % 128 == 0 && !self.recovery_fifo_blocks.is_empty() {
            if !self.recovery_ctrl_written {
                let status = self
                    .i3c_core()
                    .sec_fw_recovery_if_device_status_0
                    .read(DeviceStatus0::DevStatus);

                if status != 3 && self.bmc_step_counter % 65536 == 0 {
                    println!("Waiting for device status to be 3, currently: {}", status);
                    return;
                }

                let len = ((self.recovery_ctrl_len / 4) as u32).to_le_bytes();
                let mut ctrl = vec![0, 1];
                ctrl.extend_from_slice(&len);

                println!("Writing Indirect fifo ctrl: {:x?}", ctrl);
                self.recovery_block_write_request(RecoveryCommandCode::IndirectFifoCtrl, &ctrl);

                let reported_len = self
                    .i3c_core()
                    .sec_fw_recovery_if_indirect_fifo_ctrl_1
                    .get();

                println!("I3C core reported length: {}", reported_len);
                if reported_len as usize != self.recovery_ctrl_len / 4 {
                    println!(
                        "I3C core reported length should have been {}",
                        self.recovery_ctrl_len / 4
                    );

                    self.print_i3c_registers();

                    panic!(
                        "I3C core reported length should have been {}",
                        self.recovery_ctrl_len / 4
                    );
                }
                self.recovery_ctrl_written = true;
            }
            let fifo_status = self
                .recovery_block_read_request(RecoveryCommandCode::IndirectFifoStatus)
                .expect("Device should response to indirect fifo status read request");
            let empty = fifo_status[0] & 1 == 1;
            // while empty send
            if empty {
                // fifo is empty, send a block
                let chunk = self.recovery_fifo_blocks.pop().unwrap();
                self.blocks_sent += 1;
                self.recovery_block_write_request(RecoveryCommandCode::IndirectFifoData, &chunk);
            }
        }

        // don't run the BMC every time as it can spam requests
        if self.bmc_step_counter < 100_000 || self.bmc_step_counter % 10_000 != 0 {
            return;
        }
        self.bmc.step();

        // we need to translate from the BMC events to the I3C controller block reads and writes
        let Ok(event) = self.from_bmc.try_recv() else {
            return;
        };
        // ignore messages that aren't meant for Caliptra core.
        if !matches!(event.dest, Device::CaliptraCore) {
            return;
        }
        match event.event {
            EventData::RecoveryBlockReadRequest {
                source_addr,
                target_addr,
                command_code,
            } => {
                // println!("From BMC: Recovery block read request {:?}", command_code);

                if let Some(payload) = self.recovery_block_read_request(command_code) {
                    self.to_bmc
                        .send(Event {
                            src: Device::CaliptraCore,
                            dest: Device::BMC,
                            event: EventData::RecoveryBlockReadResponse {
                                source_addr: target_addr,
                                target_addr: source_addr,
                                command_code,
                                payload,
                            },
                        })
                        .unwrap();
                }
            }
            EventData::RecoveryBlockReadResponse {
                source_addr: _,
                target_addr: _,
                command_code: _,
                payload: _,
            } => todo!(),
            EventData::RecoveryBlockWrite {
                source_addr: _,
                target_addr: _,
                command_code,
                payload,
            } => {
                //println!("Recovery block write request: {:?}", command_code);

                self.recovery_block_write_request(command_code, &payload);
            }
            EventData::RecoveryImageAvailable { image_id: _, image } => {
                // do the indirect fifo thing
                println!("Recovery image available; writing blocks");

                self.recovery_ctrl_len = image.len();
                self.recovery_ctrl_written = false;
                // let fifo_status =
                //     self.recovery_block_read_request(RecoveryCommandCode::IndirectFifoStatus);

                let mut image = image.clone();
                while image.len() % 256 != 0 {
                    image.push(0);
                }
                self.recovery_fifo_blocks = image.chunks(256).map(|chunk| chunk.to_vec()).collect();
                self.recovery_fifo_blocks.reverse(); // reverse so we can pop from the end
            }
            _ => todo!(),
        }
    }

    fn command_code_to_u8(command: RecoveryCommandCode) -> u8 {
        match command {
            RecoveryCommandCode::ProtCap => 34,
            RecoveryCommandCode::DeviceId => 35,
            RecoveryCommandCode::DeviceStatus => 36,
            RecoveryCommandCode::DeviceReset => 37,
            RecoveryCommandCode::RecoveryCtrl => 38,
            RecoveryCommandCode::RecoveryStatus => 39,
            RecoveryCommandCode::HwStatus => 40,
            RecoveryCommandCode::IndirectCtrl => 41,
            RecoveryCommandCode::IndirectStatus => 42,
            RecoveryCommandCode::IndirectData => 43,
            RecoveryCommandCode::Vendor => 44,
            RecoveryCommandCode::IndirectFifoCtrl => 45,
            RecoveryCommandCode::IndirectFifoStatus => 46,
            RecoveryCommandCode::IndirectFifoData => 47,
        }
    }

    fn command_code_to_len(command: RecoveryCommandCode) -> (u16, u16) {
        match command {
            RecoveryCommandCode::ProtCap => (15, 15),
            RecoveryCommandCode::DeviceId => (24, 255),
            RecoveryCommandCode::DeviceStatus => (7, 255),
            RecoveryCommandCode::DeviceReset => (3, 3),
            RecoveryCommandCode::RecoveryCtrl => (3, 3),
            RecoveryCommandCode::RecoveryStatus => (2, 2),
            RecoveryCommandCode::HwStatus => (4, 255),
            RecoveryCommandCode::IndirectCtrl => (6, 6),
            RecoveryCommandCode::IndirectStatus => (6, 6),
            RecoveryCommandCode::IndirectData => (1, 252),
            RecoveryCommandCode::Vendor => (1, 255),
            RecoveryCommandCode::IndirectFifoCtrl => (6, 6),
            RecoveryCommandCode::IndirectFifoStatus => (20, 20),
            RecoveryCommandCode::IndirectFifoData => (1, 4095),
        }
    }

    fn print_i3c_registers(&mut self) {
        println!("Dumping registers");
        println!(
            "sec_fw_recovery_if_prot_cap_0: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_prot_cap_0
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_prot_cap_1: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_prot_cap_1
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_prot_cap_2: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_prot_cap_2
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_prot_cap_3: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_prot_cap_3
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_0: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_0
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_1: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_1
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_2: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_2
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_3: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_3
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_4: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_4
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_5: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_5
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_id_reserved: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_id_reserved
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_status_0: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_status_0
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_status_1: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_status_1
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_device_reset: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_device_reset
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_recovery_ctrl: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_recovery_ctrl
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_recovery_status: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_recovery_status
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_hw_status: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_hw_status
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_ctrl_0: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_ctrl_0
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_ctrl_1: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_ctrl_1
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_status_0: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_status_0
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_status_1: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_status_1
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_status_2: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_status_2
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_status_3: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_status_3
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_status_4: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_status_4
                .get()
                .swap_bytes()
        );
        println!(
            "sec_fw_recovery_if_indirect_fifo_reserved: {:08x}",
            self.i3c_core()
                .sec_fw_recovery_if_indirect_fifo_reserved
                .get()
                .swap_bytes()
        );
    }

    fn get_i3c_primary_addr(&mut self) -> u8 {
        let reg = self
            .i3c_core()
            .stdby_ctrl_mode_stby_cr_device_addr
            .extract();
        if reg.is_set(StbyCrDeviceAddr::DynamicAddrValid) {
            reg.read(StbyCrDeviceAddr::DynamicAddr) as u8
        } else if reg.is_set(StbyCrDeviceAddr::StaticAddrValid) {
            reg.read(StbyCrDeviceAddr::StaticAddr) as u8
        } else {
            panic!("I3C target does not have a valid address set");
        }
    }

    fn get_i3c_recovery_addr(&mut self) -> u8 {
        let reg = self
            .i3c_core()
            .stdby_ctrl_mode_stby_cr_virt_device_addr
            .extract();
        if reg.is_set(StbyCrVirtDeviceAddr::VirtDynamicAddrValid) {
            reg.read(StbyCrVirtDeviceAddr::VirtDynamicAddr) as u8
        } else if reg.is_set(StbyCrVirtDeviceAddr::VirtStaticAddrValid) {
            reg.read(StbyCrVirtDeviceAddr::VirtStaticAddr) as u8
        } else {
            panic!("I3C target does not have a valid address set");
        }
    }

    // send a recovery block write request to the I3C target
    pub fn send_i3c_write(&mut self, payload: &[u8]) {
        let target_addr = self.get_i3c_primary_addr();
        println!("I3C addr = {:x}", target_addr);
        let mut cmd = xi3c::Command {
            cmd_type: 1,
            no_repeated_start: 1,
            pec: 1,
            target_addr,
            ..Default::default()
        };
        println!("TTI status: {:x}", self.i3c_core().tti_status.get());
        println!(
            "TTI interrupt enable: {:x}",
            self.i3c_core().tti_interrupt_enable.get()
        );
        println!(
            "TTI interrupt status: {:x}",
            self.i3c_core().tti_interrupt_status.get()
        );
        match self
            .i3c_controller
            .master_send_polled(&mut cmd, payload, payload.len() as u16)
        {
            Ok(_) => {
                println!("Acknowledge received");
            }
            Err(e) => {
                println!("Failed to ack write message sent to target: {:x}", e);
            }
        }

        println!("TTI status: {:x}", self.i3c_core().tti_status.get());
        println!(
            "TTI interrupt enable: {:x}",
            self.i3c_core().tti_interrupt_enable.get()
        );
        println!(
            "TTI interrupt status: {:x}",
            self.i3c_core().tti_interrupt_status.get()
        );
    }

    // send a recovery block read request to the I3C target
    fn recovery_block_read_request(&mut self, command: RecoveryCommandCode) -> Option<Vec<u8>> {
        // per the recovery spec, this maps to a private write and private read

        let target_addr = self.get_i3c_recovery_addr();

        // First we write the recovery command code for the block we want
        let mut cmd = xi3c::Command {
            cmd_type: 1,
            no_repeated_start: 0, // we want the next command (read) to be Sr
            pec: 1,
            target_addr,
            ..Default::default()
        };

        let recovery_command_code = Self::command_code_to_u8(command);

        if self
            .i3c_controller
            .master_send_polled(&mut cmd, &[recovery_command_code], 1)
            .is_err()
        {
            return None;
        }

        // then we send a private read for the minimum length
        let len_range = Self::command_code_to_len(command);
        cmd.target_addr = target_addr;
        cmd.no_repeated_start = 0;
        cmd.tid = 0;
        cmd.pec = 0;
        cmd.cmd_type = 1;

        self.i3c_controller
            .master_recv(&mut cmd, len_range.0 + 2)
            .expect("Failed to receive ack from target");

        // read in the length, lsb then msb
        let resp = self
            .i3c_controller
            .master_recv_finish(
                Some(self.realtime_thread_exit_flag.clone()),
                &cmd,
                len_range.0 + 2,
            )
            .unwrap_or_else(|_| panic!("Expected to read {}+ bytes", len_range.0 + 2));

        if resp.len() < 2 {
            panic!("Expected to read at least 2 bytes from target for recovery block length");
        }
        let len = u16::from_le_bytes([resp[0], resp[1]]);
        if len < len_range.0 || len > len_range.1 {
            self.print_i3c_registers();
            panic!(
                "Reading block {:?} expected to read between {} and {} bytes from target, got {}",
                command, len_range.0, len_range.1, len
            );
        }
        let len = len as usize;
        let left = len - (resp.len() - 2);
        // read the rest of the bytes
        if left > 0 {
            // TODO: if the length is more than the minimum we need to abort and restart with the correct value
            // because the xi3c controller does not support variable reads.
            todo!()
        }
        Some(resp[2..].to_vec())
    }

    // send a recovery block write request to the I3C target
    fn recovery_block_write_request(&mut self, command: RecoveryCommandCode, payload: &[u8]) {
        // per the recovery spec, this maps to a private write

        let target_addr = self.get_i3c_recovery_addr();
        let mut cmd = xi3c::Command {
            cmd_type: 1,
            no_repeated_start: 1,
            pec: 1,
            target_addr,
            ..Default::default()
        };

        let recovery_command_code = Self::command_code_to_u8(command);

        let mut data = vec![recovery_command_code];
        data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        data.extend_from_slice(payload);

        assert!(
            self.i3c_controller
                .master_send_polled(&mut cmd, &data, data.len() as u16)
                .is_ok(),
            "Failed to ack write message sent to target"
        );
        // println!("Acknowledge received");
    }

    #[allow(dead_code)]
    fn write_lc(&mut self, offset: u32, data: &[u16]) {
        let shift = 0;
        //let offset = offset / 2;
        for (i, x) in data.iter().copied().enumerate() {
            let i = (offset as isize + i as isize * 2) << shift;
            println!("OTP memory write {:04x} = {:02x}", i, (x >> 8) & 0xff);
            unsafe { core::ptr::write_volatile(self.otp_mem_backdoor.offset(i), (x >> 8) as u8) };
            println!(
                "OTP memory write {:04x} = {:02x}",
                i + (1 << shift),
                x & 0xff
            );
            unsafe {
                core::ptr::write_volatile(
                    self.otp_mem_backdoor.offset(i + (1 << shift)),
                    (x & 0xff) as u8,
                )
            };
        }
    }

    fn otp_slice(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.otp_mem_backdoor, OTP_SIZE) }
    }

    pub fn print_otp_memory(&self) {
        let otp = self.otp_slice();
        for (i, oi) in otp.iter().copied().enumerate() {
            if oi != 0 {
                println!("OTP mem: {:03x}: {:02x}", i, oi);
            }
        }
    }

    pub fn open_openocd(&mut self, port: u16) -> Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect(addr)?;
        self.openocd = Some(stream);
        Ok(())
    }

    pub fn close_openocd(&mut self) {
        self.openocd.take();
    }

    pub fn set_uds_req(&mut self) -> Result<()> {
        let Some(mut socket) = self.openocd.take() else {
            bail!("openocd socket is not open");
        };

        socket.write_all("riscv.cpu riscv dmi_write 0x70 4\n".as_bytes())?;

        self.openocd = Some(socket);
        Ok(())
    }

    pub fn set_bootfsm_go(&mut self) -> Result<()> {
        let Some(mut socket) = self.openocd.take() else {
            bail!("openocd socket is not open");
        };

        socket.write_all("riscv.cpu riscv dmi_write 0x61 1\n".as_bytes())?;

        self.openocd = Some(socket);
        Ok(())
    }

    fn caliptra_apb_bus(&mut self) -> FpgaRealtimeBus<'_> {
        FpgaRealtimeBus {
            mmio: self.caliptra_mmio.ptr,
            phantom: Default::default(),
        }
    }
}

impl McuHwModel for ModelFpgaRealtime {
    fn step(&mut self) {
        self.handle_log();
        self.bmc_step();
    }

    fn new_unbooted(params: InitParams) -> Result<Self>
    where
        Self: Sized,
    {
        let output = Output::new(params.log_writer);
        let dev0 = UioDevice::blocking_new(0)?;
        let dev1 = UioDevice::blocking_new(1)?;
        let devs = [dev0, dev1];

        let wrapper = Arc::new(Wrapper {
            ptr: devs[FPGA_WRAPPER_MAPPING.0]
                .map_mapping(FPGA_WRAPPER_MAPPING.1)
                .map_err(fmt_uio_error)? as *mut u32,
        });
        let caliptra_rom_backdoor = devs[CALIPTRA_ROM_MAPPING.0]
            .map_mapping(CALIPTRA_ROM_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u8;
        let otp_mem_backdoor = devs[OTP_RAM_MAPPING.0]
            .map_mapping(OTP_RAM_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u8;
        let mcu_rom_backdoor = devs[MCU_ROM_MAPPING.0]
            .map_mapping(MCU_ROM_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u8;
        let mci_ptr = devs[MCI_MAPPING.0]
            .map_mapping(MCI_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;
        let caliptra_mmio = devs[CALIPTRA_MAPPING.0]
            .map_mapping(CALIPTRA_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;
        let i3c_mmio = devs[I3C_TARGET_MAPPING.0]
            .map_mapping(I3C_TARGET_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;
        let i3c_controller_mmio = devs[I3C_CONTROLLER_MAPPING.0]
            .map_mapping(I3C_CONTROLLER_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;
        let _lc_mmio = devs[LC_MAPPING.0]
            .map_mapping(LC_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;
        let _otp_mmio = devs[OTP_MAPPING.0]
            .map_mapping(OTP_MAPPING.1)
            .map_err(fmt_uio_error)? as *mut u32;

        let realtime_thread_exit_flag = Arc::new(AtomicBool::new(true));
        let realtime_thread_exit_flag2 = realtime_thread_exit_flag.clone();
        let realtime_wrapper = wrapper.clone();
        let i3c_target = unsafe { &*(i3c_mmio as *const i3c::regs::I3c) };

        let realtime_thread = Some(std::thread::spawn(move || {
            Self::realtime_thread_itrng_fn(
                realtime_wrapper,
                realtime_thread_exit_flag2,
                params.itrng_nibbles,
            )
        }));

        let i3c_controller = xi3c::Controller::new(i3c_controller_mmio);

        // For now, we copy the runtime directly into the SRAM
        let mut mcu_fw = params.mcu_firmware.to_vec();
        while mcu_fw.len() % 8 != 0 {
            mcu_fw.push(0);
        }

        let (caliptra_cpu_event_sender, from_bmc) = mpsc::channel();
        let (to_bmc, caliptra_cpu_event_recv) = mpsc::channel();

        // these aren't used
        let (mcu_cpu_event_sender, mcu_cpu_event_recv) = mpsc::channel();

        // This is a fake BMC that runs the recovery flow as a series of events for recovery block reads and writes.
        let mut bmc = Bmc::new(
            caliptra_cpu_event_sender,
            caliptra_cpu_event_recv,
            mcu_cpu_event_sender,
            mcu_cpu_event_recv,
        );
        bmc.push_recovery_image(params.caliptra_firmware.to_vec());
        bmc.push_recovery_image(params.soc_manifest.to_vec());
        bmc.push_recovery_image(params.mcu_firmware.to_vec());

        let mut m = Self {
            devs,
            wrapper,
            caliptra_mmio: CaliptraMmio { ptr: caliptra_mmio },
            caliptra_rom_backdoor,
            mcu_rom_backdoor,
            otp_mem_backdoor,
            mci: Mci { ptr: mci_ptr },
            i3c_mmio,
            i3c_controller_mmio,
            i3c_controller,
            otp_init: params.otp_memory.map(|m| m.to_vec()).unwrap_or_default(),
            lifecycle_controller_state: params.lifecycle_controller_state,
            realtime_thread,
            realtime_thread_exit_flag,

            output,
            recovery_started: false,
            bmc,
            from_bmc,
            to_bmc,
            recovery_fifo_blocks: vec![],
            bmc_step_counter: 0,
            i3c_target,
            blocks_sent: 0,
            recovery_ctrl_written: false,
            recovery_ctrl_len: 0,
            openocd: None,
        };

        // Set generic input wires.
        let input_wires = [0, (!params.uds_granularity_32 as u32) << 31];
        m.set_generic_input_wires(&input_wires);

        // Set Security State signal wires
        // println!(
        //     "Set security state to 0x{:x}",
        //     u32::from(params.security_state)
        // );
        // TODO: use lifecycle transitions for this
        // m.set_security_state(params.security_state);

        println!("Set itrng divider");
        // Set divisor for ITRNG throttling
        m.set_itrng_divider(ITRNG_DIVISOR);

        println!("Set deobf key");
        // Set deobfuscation key
        for i in 0..8 {
            m.wrapper.regs().cptra_obf_key[i].set(params.cptra_obf_key[i]);
        }

        // Set the CSR HMAC key
        for i in 0..16 {
            m.wrapper.regs().cptra_csr_hmac_key[i].set(params.csr_hmac_key[i]);
        }

        // Set the UDS Seed
        for (i, udsi) in DEFAULT_UDS_SEED.iter().copied().enumerate() {
            m.wrapper.regs().cptra_obf_uds_seed[i].set(udsi);
        }

        // Set the FE Seed
        for (i, fei) in DEFAULT_FIELD_ENTROPY.iter().copied().enumerate() {
            m.wrapper.regs().cptra_obf_field_entropy[i].set(fei);
        }

        // Currently not using strap UDS and FE
        m.set_secrets_valid(false);

        m.set_bootfsm_break(params.bootfsm_break);

        // Clear the generic input wires in case they were left in a non-zero state.
        m.set_generic_input_wires(&[0, 0]);
        m.set_mcu_generic_input_wires(&[0, 0]);

        // if params.uds_program_req {
        //     // notify MCU that we want to run the UDS provisioning flow
        //     m.set_mcu_generic_input_wires(&[1, 0]);
        // }

        println!("Putting subsystem into reset");
        m.set_subsystem_reset(true);

        println!("Clearing OTP memory");
        let otp_mem = m.otp_slice();
        otp_mem.fill(0);

        if !m.otp_init.is_empty() {
            // write the initial contents of the OTP memory
            println!("Initializing OTP with initialized data");
            if m.otp_init.len() > otp_mem.len() {
                bail!(
                    "OTP initialization data is larger than OTP memory {} > {}",
                    m.otp_init.len(),
                    otp_mem.len()
                );
            }
            otp_mem[..m.otp_init.len()].copy_from_slice(&m.otp_init);
            //m.print_otp_memory();
        }

        // TODO: support directly setting lifecycle state
        if let Some(_lifecycle_state) = m.lifecycle_controller_state {
            //println!("Setting LC state to 1");
            // let lc_cnt_start = 0xfa8;
            // let lc_state_start = 0xfd8;
            // count1.reverse();
            // test_unlocked1.reverse();
        }

        println!("Clearing fifo");
        // Sometimes there's garbage in here; clean it out
        m.clear_logs();

        println!("new_unbooted");

        // Set initial PAUSER
        m.set_axi_user(DEFAULT_AXI_PAUSER);

        println!("AXI user written {:x}", DEFAULT_AXI_PAUSER);

        // Write ROM images over backdoors
        // ensure that they are 8-byte aligned to write to AXI
        let mut caliptra_rom_data = params.caliptra_rom.to_vec();
        while caliptra_rom_data.len() % 8 != 0 {
            caliptra_rom_data.push(0);
        }
        let mut mcu_rom_data = params.mcu_rom.to_vec();
        while mcu_rom_data.len() % 8 != 0 {
            mcu_rom_data.push(0);
        }

        // copy the ROM data
        let caliptra_rom_slice = unsafe {
            core::slice::from_raw_parts_mut(m.caliptra_rom_backdoor, caliptra_rom_data.len())
        };
        println!("Writing Caliptra ROM ({} bytes)", caliptra_rom_data.len());
        caliptra_rom_slice.copy_from_slice(&caliptra_rom_data);
        println!("Writing MCU ROM");
        let mcu_rom_slice =
            unsafe { core::slice::from_raw_parts_mut(m.mcu_rom_backdoor, mcu_rom_data.len()) };
        mcu_rom_slice.copy_from_slice(&mcu_rom_data);

        // set the reset vector to point to the ROM backdoor
        println!("Writing MCU reset vector");
        m.wrapper
            .regs()
            .mcu_reset_vector
            .set(mcu_config_fpga::FPGA_MEMORY_MAP.rom_offset);

        println!("Taking subsystem out of reset");
        m.set_subsystem_reset(false);

        // println!(
        //     "Mode {}",
        //     if (m.caliptra_mmio.soc().cptra_hw_config.get() >> 5) & 1 == 1 {
        //         "subsystem"
        //     } else {
        //         "passive"
        //     }
        // );

        // TODO: remove this when we can finish subsystem/active mode
        // println!("Writing MCU firmware to SRAM");
        // // For now, we copy the runtime directly into the SRAM
        // let mut fw_data = params.mcu_firmware.to_vec();
        // while fw_data.len() % 8 != 0 {
        //     fw_data.push(0);
        // }
        // // TODO: remove this offset 0x80 and add 128 bytes of padding to the beginning of the firmware
        // // as this is going to fail when we use the DMA controller
        // let sram_slice = unsafe {
        //     core::slice::from_raw_parts_mut(m.mcu_sram_backdoor.offset(0x80), fw_data.len())
        // };
        // sram_slice.copy_from_slice(&fw_data);

        println!("Done starting MCU");
        Ok(m)
    }

    fn type_name(&self) -> &'static str {
        "ModelFpgaRealtime"
    }

    fn output(&mut self) -> &mut crate::Output {
        let cycle = self.wrapper.regs().cycle_count.get();
        self.output.sink().set_now(u64::from(cycle));
        &mut self.output
    }

    fn ready_for_fw(&self) -> bool {
        true
    }

    fn tracing_hint(&mut self, _enable: bool) {
        // Do nothing; we don't support tracing yet
    }

    fn set_axi_user(&mut self, pauser: u32) {
        self.wrapper.regs().arm_user.set(pauser);
        self.wrapper.regs().lsu_user.set(pauser);
        self.wrapper.regs().ifu_user.set(pauser);
        self.wrapper.regs().dma_axi_user.set(pauser);
        self.wrapper.regs().soc_config_user.set(pauser);
        self.wrapper.regs().sram_config_user.set(pauser);
    }

    fn set_caliptra_boot_go(&mut self, go: bool) {
        self.mci
            .regs()
            .mci_reg_cptra_boot_go
            .write(Go.val(go as u32));
    }

    fn set_itrng_divider(&mut self, divider: u32) {
        self.wrapper.regs().itrng_divisor.set(divider - 1);
    }

    fn set_security_state(&mut self, _value: SecurityState) {
        // todo!() // this is no yet supported in FPGA
    }

    fn set_generic_input_wires(&mut self, value: &[u32; 2]) {
        for (i, wire) in value.iter().copied().enumerate() {
            self.wrapper.regs().generic_input_wires[i].set(wire);
        }
    }

    fn set_mcu_generic_input_wires(&mut self, value: &[u32; 2]) {
        for (i, wire) in value.iter().copied().enumerate() {
            self.wrapper.regs().mci_generic_input_wires[i].set(wire);
        }
    }

    fn events_from_caliptra(&mut self) -> Vec<Event> {
        todo!()
    }

    fn events_to_caliptra(&mut self) -> mpsc::Sender<Event> {
        todo!()
    }

    fn cycle_count(&mut self) -> u64 {
        self.wrapper.regs().cycle_count.get() as u64
    }

    fn save_otp_memory(&self, path: &Path) -> Result<()> {
        let s = crate::vmem::write_otp_vmem_data(self.otp_slice())?;
        Ok(std::fs::write(path, s.as_bytes())?)
    }

    fn caliptra_soc_manager(&mut self) -> impl SocManager {
        self
    }
}

pub struct FpgaRealtimeBus<'a> {
    mmio: *mut u32,
    phantom: PhantomData<&'a mut ()>,
}

impl<'a> FpgaRealtimeBus<'a> {
    fn ptr_for_addr(&mut self, addr: RvAddr) -> Option<*mut u32> {
        let addr = addr as usize;
        unsafe {
            match addr {
                0x3002_0000..=0x3003_ffff => Some(self.mmio.add((addr - 0x3000_0000) / 4)),
                _ => None,
            }
        }
    }
}

impl<'a> Bus for FpgaRealtimeBus<'a> {
    fn read(&mut self, _size: RvSize, addr: RvAddr) -> Result<RvData, BusError> {
        if let Some(ptr) = self.ptr_for_addr(addr) {
            Ok(unsafe { ptr.read_volatile() })
        } else {
            println!("Error LoadAccessFault");
            Err(BusError::LoadAccessFault)
        }
    }

    fn write(&mut self, _size: RvSize, addr: RvAddr, val: RvData) -> Result<(), BusError> {
        if let Some(ptr) = self.ptr_for_addr(addr) {
            // TODO: support 16-bit and 8-bit writes
            unsafe { ptr.write_volatile(val) };
            Ok(())
        } else {
            Err(BusError::StoreAccessFault)
        }
    }
}

impl SocManager for &mut ModelFpgaRealtime {
    const SOC_IFC_ADDR: u32 = 0x3003_0000;
    const SOC_IFC_TRNG_ADDR: u32 = 0x3003_0000;
    const SOC_MBOX_ADDR: u32 = 0x3002_0000;

    const MAX_WAIT_CYCLES: u32 = 20_000_000;

    type TMmio<'a>
        = BusMmio<FpgaRealtimeBus<'a>>
    where
        Self: 'a;

    fn mmio_mut(&mut self) -> Self::TMmio<'_> {
        BusMmio::new(self.caliptra_apb_bus())
    }

    fn delay(&mut self) {
        self.step();
    }
}

impl Drop for ModelFpgaRealtime {
    fn drop(&mut self) {
        self.realtime_thread_exit_flag
            .store(false, Ordering::Relaxed);
        self.realtime_thread.take().unwrap().join().unwrap();
        self.close_openocd();
        self.i3c_controller.off();

        self.set_generic_input_wires(&[0, 0]);
        self.set_mcu_generic_input_wires(&[0, 0]);

        // ensure that we put the I3C target into a state where we will reset it properly
        self.i3c_target.stdby_ctrl_mode_stby_cr_device_addr.set(0);
        self.set_subsystem_reset(true);

        // Unmap UIO memory space so that the file lock is released
        self.unmap_mapping(self.wrapper.ptr, FPGA_WRAPPER_MAPPING);
        self.unmap_mapping(self.caliptra_mmio.ptr, CALIPTRA_MAPPING);
        self.unmap_mapping(self.caliptra_rom_backdoor as *mut u32, CALIPTRA_ROM_MAPPING);
        self.unmap_mapping(self.mcu_rom_backdoor as *mut u32, MCU_ROM_MAPPING);
        self.unmap_mapping(self.otp_mem_backdoor as *mut u32, OTP_RAM_MAPPING);
        self.unmap_mapping(self.mci.ptr, MCI_MAPPING);
        self.unmap_mapping(self.i3c_mmio, I3C_TARGET_MAPPING);
        self.unmap_mapping(self.i3c_controller_mmio, I3C_CONTROLLER_MAPPING);
    }
}

#[cfg(test)]
mod test {
    use crate::{DefaultHwModel, InitParams, McuHwModel};

    #[ignore]
    #[test]
    fn test_new_unbooted() {
        let mcu_rom = mcu_builder::rom_build(Some("fpga"), "").expect("Could not build MCU ROM");
        let mcu_runtime = &mcu_builder::runtime_build_with_apps_cached(
            &[],
            Some("fpga-runtime.bin"),
            false,
            Some("fpga"),
            Some(&mcu_config_fpga::FPGA_MEMORY_MAP),
            false,
            None,
            None,
            None,
        )
        .expect("Could not build MCU runtime");
        let mut caliptra_builder = mcu_builder::CaliptraBuilder::new(
            true,
            None,
            None,
            None,
            None,
            Some(mcu_runtime.into()),
            None,
        );
        let caliptra_rom = caliptra_builder
            .get_caliptra_rom()
            .expect("Could not build Caliptra ROM");
        let caliptra_fw = caliptra_builder
            .get_caliptra_fw()
            .expect("Could not build Caliptra FW bundle");
        // TODO: pass this in to the MCU through the OTP
        let vendor_pk_hash = caliptra_builder
            .get_vendor_pk_hash()
            .expect("Could not get vendor PK hash");
        println!("Vendor PK hash: {:x?}", vendor_pk_hash);
        let soc_manifest = caliptra_builder
            .get_soc_manifest()
            .expect("Could not get SOC manifest");
        use tock_registers::interfaces::Readable;

        let caliptra_rom = std::fs::read(caliptra_rom).unwrap();
        let caliptra_fw = std::fs::read(caliptra_fw).unwrap();
        let mcu_rom = std::fs::read(mcu_rom).unwrap();
        let mcu_runtime = std::fs::read(mcu_runtime).unwrap();
        let soc_manifest = std::fs::read(soc_manifest).unwrap();

        let mut model = DefaultHwModel::new_unbooted(InitParams {
            caliptra_rom: &caliptra_rom,
            caliptra_firmware: &caliptra_fw,
            mcu_rom: &mcu_rom,
            mcu_firmware: &mcu_runtime,
            soc_manifest: &soc_manifest,
            active_mode: true,
            ..Default::default()
        })
        .unwrap();
        println!("Waiting on I3C target to be configured");
        let mut xi3c_configured = false;
        for _ in 0..2_000_000 {
            model.step();
            if !xi3c_configured && model.i3c_target_configured() {
                xi3c_configured = true;
                println!("I3C target configured");
                model.configure_i3c_controller();
                println!("Starting recovery flow (BMC)");
                println!(
                    "Mode {}",
                    if (model.caliptra_mmio.soc().cptra_hw_config.get() >> 5) & 1 == 1 {
                        "subsystem"
                    } else {
                        "passive"
                    }
                );

                model.start_recovery_bmc();
            }
        }

        println!("Ending");
    }
}

/*++

Licensed under the Apache-2.0 license.

File Name:

    cold_boot.rs

Abstract:

    Cold Boot Flow - Handles initial boot when MCU powers on

--*/

#![allow(clippy::empty_loop)]

use crate::boot_status::McuRomBootStatus;
use crate::{fatal_error, BootFlow, RomEnv, RomParameters};
use core::fmt::Write;
use romtime::HexWord;
use tock_registers::interfaces::Readable;

pub struct WarmBoot {}

impl BootFlow for WarmBoot {
    fn run(env: &mut RomEnv, _params: RomParameters) -> ! {
        romtime::println!("[mcu-rom] Starting warm boot flow");
        env.mci
            .set_flow_status(McuRomBootStatus::WarmResetFlowStarted.into());

        // Create local references to minimize code changes
        let mci = &env.mci;
        let soc = &env.soc;
        let lc = &env.lc;
        let otp = &mut env.otp;
        let i3c = &mut env.i3c;
        let straps = &env.straps;

        romtime::println!("[mcu-rom] Setting Caliptra boot go");
        mci.caliptra_boot_go();
        mci.set_flow_status(McuRomBootStatus::CaliptraBootGoAsserted.into());

        // If testing Caliptra Core, hang here until the test signals it to continue.
        if cfg!(feature = "core_test") {
            while mci.registers.mci_reg_generic_input_wires[1].get() & (1 << 30) == 0 {}
        }

        lc.init().unwrap();
        mci.set_flow_status(McuRomBootStatus::LifecycleControllerInitialized.into());

        // FPGA has problems with the integrity check, so we disable it
        if let Err(err) = otp.init() {
            romtime::println!("[mcu-rom] Error initializing OTP: {}", HexWord(err as u32));
            fatal_error(err as u32);
        }
        mci.set_flow_status(McuRomBootStatus::OtpControllerInitialized.into());

        let _fuses = match otp.read_fuses() {
            Ok(fuses) => {
                mci.set_flow_status(McuRomBootStatus::FusesReadFromOtp.into());
                fuses
            }
            Err(e) => {
                romtime::println!("Error reading fuses: {}", HexWord(e as u32));
                fatal_error(1);
            }
        };

        romtime::println!("[mcu-rom] Initializing I3C");
        i3c.configure(straps.i3c_static_addr, true);
        mci.set_flow_status(McuRomBootStatus::I3cInitialized.into());

        romtime::println!(
            "[mcu-rom] Waiting for Caliptra to be ready for fuses: {}",
            soc.ready_for_fuses()
        );
        while !soc.ready_for_fuses() {}
        mci.set_flow_status(McuRomBootStatus::CaliptraReadyForFuses.into());

        romtime::println!("[mcu-rom] Setting Caliptra fuse write done");
        soc.fuse_write_done();
        while soc.ready_for_fuses() {}
        mci.set_flow_status(McuRomBootStatus::FuseWriteComplete.into());

        // If testing Caliptra Core, hang here until the test signals it to continue.
        if cfg!(feature = "core_test") {
            while mci.registers.mci_reg_generic_input_wires[1].get() & (1 << 31) == 0 {}
        }

        // TODO: Implement full warm reset flow
        romtime::println!("[mcu-rom] TODO: Warm reset flow not fully implemented");
        fatal_error(0x1001); // Error code for unimplemented warm reset
    }
}

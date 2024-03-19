#![no_std]
#![no_main]

use core::future::{self};

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::can::frame::Header;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals::*;
use embassy_stm32::rcc::mux;
use embassy_stm32::{bind_interrupts, can, rcc, Config};

use embassy_time::Timer;
use embedded_can::{ExtendedId, Id};
use futures::future::{join, join3, join4};
use futures::stream::{self, unfold};
use futures::{StreamExt, TryStreamExt};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    FDCAN1_IT0 => can::IT0InterruptHandler<FDCAN1>;
    FDCAN1_IT1 => can::IT1InterruptHandler<FDCAN1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("Hello CAN!");

    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hsi = Some(HSIPrescaler::DIV1); // 64 Mhz
        config.rcc.csi = true;
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: true }); // needed for USB
        config.rcc.pll1 = Some(Pll {
            source: PllSource::HSI,   // 64 Mhz
            prediv: PllPreDiv::DIV4,  // 16 Mhz, this has to be between 2 Mhz and 16 Mhz
            mul: PllMul::MUL48,       // 768 Mhz
            divp: Some(PllDiv::DIV2), // 384 Mhz, these dividers have to be at least 2 to create a 50/50 duty cycle (the VCO from the PLL does not guarantee that)
            divq: Some(PllDiv::DIV8), // 96 Mhz
            divr: None,
        });
        config.rcc.sys = Sysclk::PLL1_P; // 384 Mhz
        config.rcc.ahb_pre = AHBPrescaler::DIV2; // 192 Mhz
        config.rcc.apb1_pre = APBPrescaler::DIV2; // 96 Mhz
        config.rcc.apb2_pre = APBPrescaler::DIV2; // 96 Mhz
        config.rcc.apb3_pre = APBPrescaler::DIV2; // 96 Mhz
        config.rcc.apb4_pre = APBPrescaler::DIV2; // 96 Mhz
        config.rcc.voltage_scale = VoltageScale::Scale1;
        config.rcc.mux.fdcansel = mux::Fdcansel::PLL1_Q;
    }

    let peripherals = embassy_stm32::init(config);

    let mut can = can::FdcanConfigurator::new(peripherals.FDCAN1, peripherals.PA11, peripherals.PA12, Irqs);

    // 250k bps
    can.set_bitrate(500_000);
    can.set_fd_data_bitrate(4_000_000, true);

    let mut can = can.into_internal_loopback_mode();
    // let mut can = can.into_normal_mode();

    info!("CAN Configured");

    let mut i = 0u8;
    let mut last_read_ts = embassy_time::Instant::now();

    // loop {
    //     // let frame = can::frame::ClassicFrame::new_extended(0x123456F, &[i; 8]).unwrap();
    //     // info!("Writing frame");
    //     // _ = can.write(&frame).await;

    //     let fd_frame = can::frame::FdFrame::new(
    //         Header::new_fd(Id::Extended(ExtendedId::new(123456).unwrap()), 8, false, true),
    //         &[i; 8],
    //     )
    //     .unwrap();
    //     _ = can.write_fd(&fd_frame).await;

    //     match can.read().await {
    //         Ok((rx_frame, ts)) => {
    //             let delta = (ts - last_read_ts).as_millis();
    //             last_read_ts = ts;

    //             let id = match rx_frame.id() {
    //                 Id::Standard(id) => (id.as_raw() as u32),
    //                 Id::Extended(id) => (id.as_raw()),
    //             };

    //             let fd = rx_frame.header().fdcan();

    //             info!(
    //                 "Rx: id:{:?} fd:{:?} {:x} {:x} {:x} {:x} --- NEW {}",
    //                 id,
    //                 fd,
    //                 rx_frame.data()[0],
    //                 rx_frame.data()[1],
    //                 rx_frame.data()[2],
    //                 rx_frame.data()[3],
    //                 delta,
    //             )
    //         }
    //         Err(_err) => error!("Error in frame"),
    //     }

    //     Timer::after_millis(250).await;

    //     i += 1;
    //     if i > 3 {
    //         break;
    //     }
    // }

    let (mut tx, rx, tx_event, control) = can.split_with_control();

    // let rx = &mut rx;
    //    let asy = async{
    //         rx.read()
    //    };

    // let read_fut = unfold((), |_| async { Some((rx.read().await, ())) });

    // let readfd_fut = unfold((), |_| async { Some((rx.read_fd().await, ())) });
    let read_fut = unfold(rx, |mut rx| async { Some((rx.read_fd().await, rx)) });

    let handle_fut = read_fut.for_each(|msg| {
        match msg {
            Ok((rx_frame, ts)) => {
                let delta = (ts - last_read_ts).as_millis();
                last_read_ts = ts;
                info!(
                    "Rx: {:x} {:x} {:x} {:x} --- NEW {}",
                    rx_frame.data()[0],
                    rx_frame.data()[1],
                    rx_frame.data()[2],
                    rx_frame.data()[3],
                    delta,
                )
            }
            Err(_err) => error!("Error in frame"),
        }
        future::ready(())
    });

    let write_fut = async {
        loop {
            let data = &[i; 8];
            let frame = can::frame::FdFrame::new(
                Header::new_fd(
                    Id::Extended(ExtendedId::new(123456).unwrap()),
                    data.len() as u8,
                    false,
                    true,
                ),
                data,
            )
            .unwrap();
            info!("Writing frame");
            _ = tx.write_fd_with_marker(&frame, Some(i % 10)).await;

            Timer::after_millis(500).await;

            i = i.overflowing_add(1).0;
        }
    };

    let tx_evet_fut = unfold(tx_event, |mut tx_event| async {
        Some((tx_event.read_tx_event().await, tx_event))
    });

    let tx_evet_handler = tx_evet_fut.for_each(|(header, marker, ts)| {
        // let delta = (ts - last_read_ts).as_millis();
        // last_read_ts = ts;
        info!("Tx Event with marker {} at {}", marker, ts);

        future::ready(())
    });

    let mut led = Output::new(peripherals.PB14, Level::High, Speed::Low);

    let blinky = async {
        loop {
            info!("high");
            led.set_high();
            Timer::after_millis(500).await;

            info!("low");
            led.set_low();
            Timer::after_millis(500).await;
        }
    };

    join4(write_fut, handle_fut,tx_evet_handler, blinky).await;

    // Wih split

    // loop {
    //     let frame = can::frame::ClassicFrame::new_extended(0x123456F, &[i; 8]).unwrap();
    //     info!("Writing frame");
    //     _ = tx.write(&frame).await;

    //     match rx.read().await {
    //         Ok((rx_frame, ts)) => {
    //             let delta = (ts - last_read_ts).as_millis();
    //             last_read_ts = ts;
    //             info!(
    //                 "Rx: {:x} {:x} {:x} {:x} --- NEW {}",
    //                 rx_frame.data()[0],
    //                 rx_frame.data()[1],
    //                 rx_frame.data()[2],
    //                 rx_frame.data()[3],
    //                 delta,
    //             )
    //         }
    //         Err(_err) => error!("Error in frame"),
    //     }

    //     Timer::after_millis(250).await;

    //     i += 1;
    // }
}

//! This example shows how to use RTC (Real Time Clock) in the RP2040 chip.

#![no_std]
#![no_main]

use core::str::from_utf8;

use chrono::{DateTime as ChronoDateTime, Datelike, Timelike};
use embassy_executor::Spawner;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_rp::{
    clocks::RoscRng,
    rtc::{DayOfWeek, Rtc},
};
use embassy_time::{Duration, Timer};
use log::{error, info};
use rand::RngCore;
use reqwless::{
    client::{HttpClient, TlsConfig, TlsVerify},
    request::Method,
};
use serde::Deserialize;
use {defmt_rtt as _, panic_probe as _};

mod usblogger;
mod wifi;

const WIFI_NETWORK: &str = "Bill Wi the Science Fi";
const WIFI_PASSWORD: &str = "sciencerules";

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = include_bytes!("cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("cyw43-firmware/43439A0_clm.bin");

    setup_logger!(spawner, p);
    let stack = setup_wifi!(spawner, p, fw, clm, WIFI_NETWORK, WIFI_PASSWORD);

    let mut rng = RoscRng;
    let seed = rng.next_u64();
    let mut rtc = Rtc::new(p.RTC);

    loop {
        let mut rx_buffer = [0; 8192];
        let mut tls_read_buffer = [0; 16640];
        let mut tls_write_buffer = [0; 16640];

        let client_state = TcpClientState::<1, 1024, 1024>::new();
        let tcp_client = TcpClient::new(stack, &client_state);
        let dns_client = DnsSocket::new(stack);
        let tls_config = TlsConfig::new(
            seed,
            &mut tls_read_buffer,
            &mut tls_write_buffer,
            TlsVerify::None,
        );

        let mut http_client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);
        let url = "https://worldtimeapi.org/api/timezone/America/Chicago";

        info!("connecting to {}", &url);

        let mut request = match http_client.request(Method::GET, &url).await {
            Ok(req) => req,
            Err(e) => {
                error!("Failed to make HTTP request: {:?}", e);
                return; // handle the error
            }
        };

        let response = match request.send(&mut rx_buffer).await {
            Ok(resp) => resp,
            Err(_e) => {
                error!("Failed to send HTTP request");
                return; // handle the error;
            }
        };

        let body = match from_utf8(response.body().read_to_end().await.unwrap()) {
            Ok(b) => b,
            Err(_e) => {
                error!("Failed to read response body");
                return; // handle the error
            }
        };
        info!("Response body: {:?}", &body);

        // parse the response body and update the RTC

        #[derive(Deserialize)]
        struct ApiResponse<'a> {
            datetime: &'a str,
            // other fields as needed
        }

        let bytes = body.as_bytes();
        match serde_json_core::de::from_slice::<ApiResponse>(bytes) {
            Ok((output, _used)) => {
                info!("Datetime: {:?}", output.datetime);

                // Parse the datetime string
                let datetime_str = output.datetime;
                let datetime = ChronoDateTime::parse_from_rfc3339(datetime_str).unwrap();

                let day_of_week_num = datetime.weekday().num_days_from_sunday() as u8;
                let day_of_week = match day_of_week_num {
                    0 => DayOfWeek::Sunday,
                    1 => DayOfWeek::Monday,
                    2 => DayOfWeek::Tuesday,
                    3 => DayOfWeek::Wednesday,
                    4 => DayOfWeek::Thursday,
                    5 => DayOfWeek::Friday,
                    6 => DayOfWeek::Saturday,
                    _ => return, // handle the error
                };
                let rtc_datetime = embassy_rp::rtc::DateTime {
                    year: datetime.year() as u16,
                    month: datetime.month() as u8,
                    day: datetime.day() as u8,
                    day_of_week,
                    hour: datetime.hour() as u8,
                    minute: datetime.minute() as u8,
                    second: datetime.second() as u8,
                };

                info!("RTC updated to: {:?}", &rtc_datetime);
                rtc.set_datetime(rtc_datetime).unwrap();
            }
            Err(_e) => {
                error!("Failed to parse response body");
                return; // handle the error
            }
        }

        for _ in 0..3600 {
            let current_time = rtc.now().unwrap();
            info!(
                "Current RTC time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                current_time.year,
                current_time.month,
                current_time.day,
                current_time.hour,
                current_time.minute,
                current_time.second
            );
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}
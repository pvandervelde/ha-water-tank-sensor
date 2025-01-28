// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions for module sleep

use log::info;

use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::TimerWakeupSource;
use esp_hal::rtc_cntl::Rtc;

/// Enter deep sleep for the specified interval
///
/// **NOTE**: WiFi must be turned off before entering deep sleep, otherwise
/// it will block indefinitely.
pub fn enter_deep(rtc_cntl: LPWR, interval: hifitime::Duration) -> ! {
    let wakeup_source =
        TimerWakeupSource::new(core::time::Duration::from_secs(interval.to_seconds() as u64));

    let mut rtc = Rtc::new(rtc_cntl);

    info!("Entering deep sleep for {interval:?}");
    rtc.sleep_deep(&[&wakeup_source]);
}

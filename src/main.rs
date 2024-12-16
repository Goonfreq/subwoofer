use audio_visualizer::dynamic::live_input::AudioDevAndCfg;
use audio_visualizer::dynamic::window_top_btm::{open_window_connect_audio, TransformFn};

use buttplug::{
    client::{ButtplugClient, ScalarValueCommand},
    core::connector::new_json_ws_client_connector,
};
use core::panic;
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device,
};
use lowpass_filter::lowpass_filter;
use std::io::{stdin, BufRead};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;
use tokio::{sync::mpsc, time};

/// For now, a maximum of 16 persisted samples at any given run is good enough to average.
const SAMPLE_LIMIT: usize = 16;

/// A global transmit channel used to send
// TODO(spotlightishere): This is obviously unsafe, and should be removed with a GUI redesign.
// It is used to work around the lack of local variable usage within the TransformFn closure.
static GLOBAL_TX: OnceLock<Sender<f64>> = OnceLock::new();

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let connector = new_json_ws_client_connector("ws://localhost:12345/buttplug");
    let client = ButtplugClient::new("subwoofer");

    // TODO(spotlightishere): Properly handle errors if scanning fails
    client.connect(connector).await?;
    client.start_scanning().await?;
    client.stop_scanning().await?;

    // Get all devices connected to Buttplug
    let all_devices = client.devices();
    if all_devices.is_empty() {
        panic!("No devices connected!");
    }

    // Print out all connected devices
    println!("Connected devices:");
    for device in &all_devices {
        println!(" - {}", device.name());
    }

    // We'll utilize Tokio channels to communicate between our audio analysis and vibration threads.
    let (tx, mut rx) = mpsc::channel::<f64>(SAMPLE_LIMIT);
    let _ = GLOBAL_TX.get_or_init(|| tx);

    let default_out_dev = select_output_dev();
    let default_out_config = default_out_dev.default_output_config().unwrap().config();
    let default_dev_name = default_out_dev.name()?;
    println!("Using default output device: {}", default_dev_name);

    tokio::spawn(async move {
        open_window_connect_audio(
            "Live Audio Lowpass Filter View",
            None,
            None,
            None,
            None,
            "time (seconds)",
            "Amplitude (with Lowpass filter)",
            AudioDevAndCfg::new(Some(default_out_dev), Some(default_out_config)),
            TransformFn::Basic(|direct_values: &[f32], sampling_rate: f32| {
                // Apply our lowpass filter prior to any other processing
                let mut raw_values = direct_values.to_vec();
                lowpass_filter(&mut raw_values, sampling_rate, 80.0);

                // We'll sample exactly the first frequency and adjust for vibration intensity.
                let mut first_freq: f64 = *raw_values.last().unwrap() as f64;
                first_freq = f64::abs(first_freq);
                first_freq *= 10.0;

                // Lastly, broadcast our adjusted first value!
                let Some(global_tx) = GLOBAL_TX.get() else {
                    println!("Failed to get global TX...");
                    return raw_values;
                };

                if let Err(TrySendError::Closed(_)) = global_tx.try_send(first_freq) {
                    println!("Error while sending to channel... closed!");
                }

                raw_values
            }),
        );
    });

    // We'll now loop over our sent channel values at a fixed rate of 35 ms.
    let mut interval = time::interval(Duration::from_millis(35));
    loop {
        let mut collected_values: Vec<f64> = Vec::with_capacity(SAMPLE_LIMIT);
        let result = rx.recv_many(&mut collected_values, SAMPLE_LIMIT).await;

        if result == 0 {
            println!("Detected end of tx!");
            break;
        }

        // Average our values.
        let collected_length = collected_values.len();
        let mean_value: f64 = collected_values.iter().sum::<f64>() / collected_length as f64;
        let computed_intensity = f64::min(mean_value, 1.0);

        // Send vibration commands to each device
        for device in &all_devices {
            if let Err(err) = device.vibrate(&ScalarValueCommand::ScalarValue(computed_intensity)).await {
                eprintln!("Failed to send vibration to device {}: {:?}", device.name(), err);
            }
        }

        interval.tick().await;
    }

    client.disconnect().await?;

    Ok(())
}

/// Helps to select available output devices.
pub fn list_output_devs() -> Vec<(String, cpal::Device)> {
    let host = cpal::default_host();
    type DeviceName = String;
    let mut devs: Vec<(DeviceName, Device)> = host
        .output_devices()
        .unwrap()
        .map(|dev| {
            (
                dev.name().unwrap_or_else(|_| String::from("<unknown>")),
                dev,
            )
        })
        .collect();
    devs.sort_by(|(n1, _), (n2, _)| n1.cmp(n2));
    devs
}

/// Helps to select the default output device.
fn select_output_dev() -> cpal::Device {
    let mut devs = list_output_devs();
    assert!(!devs.is_empty(), "no output devices found!");
    if devs.len() == 1 {
        return devs.remove(0).1;
    }
    println!("Type the number of the output device audio is playing to, and press enter.");
    devs.iter().enumerate().for_each(|(i, (name, dev))| {
        println!(
            "  [{}] {} {:?}",
            i,
            name,
            dev.default_output_config().unwrap()
        );
    });
    let mut input = String::new();
    stdin().lock().read_line(&mut input).unwrap();
    let index = input[0..1].parse::<usize>().unwrap();
    devs.remove(index).1
}

use anyhow::{Result, anyhow};
use rodio::cpal::traits::{DeviceTrait, HostTrait};

/// One output device: its stable id and a readable name.
#[derive(Debug, Clone)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// The output devices of the default audio host.
pub fn list_devices() -> Result<Vec<OutputDevice>> {
    let host = rodio::cpal::default_host();
    let default_id = host
        .default_output_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());
    let devices = host.output_devices()?;
    Ok(devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d
                .description()
                .map(|desc| desc.to_string())
                .unwrap_or_else(|_| id.clone());
            Some(OutputDevice {
                default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect())
}

/// The device whose id or name is `wanted` (exact match first, then a unique partial name match).
pub fn find_device(wanted: &str) -> Result<rodio::cpal::Device> {
    let host = rodio::cpal::default_host();
    let devices: Vec<_> = host.output_devices()?.collect();
    let id_of = |d: &rodio::cpal::Device| d.id().map(|id| id.to_string()).unwrap_or_default();
    let name_of = |d: &rodio::cpal::Device| d.description().map(|desc| desc.to_string()).unwrap_or_default();

    if let Some(d) = devices.iter().find(|d| id_of(d) == wanted || name_of(d) == wanted) {
        return Ok(d.clone());
    }
    let lower = wanted.to_lowercase();
    let partial: Vec<_> = devices
        .iter()
        .filter(|d| name_of(d).to_lowercase().contains(&lower))
        .collect();
    match partial.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(anyhow!(
            "no output device matches \"{wanted}\" (see `psysonic-headless devices`)"
        )),
        _ => Err(anyhow!("\"{wanted}\" matches several output devices; use its id")),
    }
}

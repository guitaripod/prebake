#![allow(unsafe_code)]

use anyhow::{Context, Result, bail};
use ash::vk;
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A Vulkan device as `fossilize_replay --device-index` sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub driver_version: u32,
    pub discrete: bool,
    pub uuid: String,
}

/// Lists Vulkan devices in a short-lived child process, so the graphics driver is never
/// loaded into the long-running daemon.
pub fn probe_in_child() -> Result<Vec<Device>> {
    let exe = std::env::current_exe().context("locating the prebake executable")?;
    let mut child = Command::new(exe)
        .args(["gpu", "--json"])
        .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
        .env("DISABLE_MANGOHUD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running prebake gpu")?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the Vulkan driver did not answer within {} s", PROBE_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("prebake gpu failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    serde_json::from_slice(&output.stdout).context("reading prebake gpu output")
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Picks the device to replay on: the configured index, else the first discrete GPU,
/// else the first device.
pub fn choose(devices: &[Device], configured: Option<u32>) -> Option<&Device> {
    match configured {
        Some(index) => devices.iter().find(|d| d.index == index),
        None => devices.iter().find(|d| d.discrete).or_else(|| devices.first()),
    }
}

/// Lists Vulkan devices in enumeration order. Loads the Vulkan loader and driver into the
/// current process; the daemon calls [`probe_in_child`] instead.
pub fn probe() -> Result<Vec<Device>> {
    let entry = unsafe { ash::Entry::load() }.context("loading libvulkan.so.1")?;
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"prebake")
        .api_version(vk::make_api_version(0, 1, 1, 0));
    let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = unsafe { entry.create_instance(&create_info, None) }.context("creating a Vulkan instance")?;
    let result = unsafe { instance.enumerate_physical_devices() }.map(|handles| {
        handles
            .iter()
            .enumerate()
            .map(|(index, handle)| {
                let props = unsafe { instance.get_physical_device_properties(*handle) };
                Device {
                    index: index as u32,
                    name: props
                        .device_name_as_c_str()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    vendor_id: props.vendor_id,
                    device_id: props.device_id,
                    driver_version: props.driver_version,
                    discrete: props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU,
                    uuid: fossilize_uuid(&props.pipeline_cache_uuid),
                }
            })
            .collect()
    });
    unsafe { instance.destroy_instance(None) };
    result.context("enumerating Vulkan devices")
}

/// Formats a pipeline cache UUID the way `fossilize_replay` names its replayer cache:
/// each byte as two hex digits, low nibble first.
pub fn fossilize_uuid(uuid: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    uuid.iter()
        .flat_map(|byte| [HEX[usize::from(byte & 0xf)] as char, HEX[usize::from(byte >> 4)] as char])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_uuids_low_nibble_first_like_fossilize() {
        let mut uuid = [0u8; 16];
        uuid[0] = 0x7d;
        uuid[1] = 0x65;
        uuid[15] = 0x9b;
        let text = fossilize_uuid(&uuid);
        assert_eq!(text.len(), 32);
        assert!(text.starts_with("d756"));
        assert!(text.ends_with("b9"));
    }

    #[test]
    fn prefers_the_configured_index_then_a_discrete_gpu() {
        let device = |index, discrete| Device {
            index,
            name: format!("gpu{index}"),
            vendor_id: 0,
            device_id: 0,
            driver_version: 0,
            discrete,
            uuid: String::new(),
        };
        let devices = vec![device(0, false), device(1, true)];
        assert_eq!(choose(&devices, None).unwrap().index, 1);
        assert_eq!(choose(&devices, Some(0)).unwrap().index, 0);
        assert!(choose(&devices, Some(5)).is_none());
        assert_eq!(choose(&devices[..1], None).unwrap().index, 0);
    }
}

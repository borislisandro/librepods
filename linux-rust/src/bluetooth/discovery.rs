use bluer::Adapter;
use log::debug;
use std::io::Error;

pub(crate) async fn find_connected_airpods(adapter: &Adapter) -> bluer::Result<bluer::Device> {
    let target_uuid = uuid::Uuid::parse_str("74ec2172-0bad-4d01-8f77-997b2be0722a").unwrap();

    let addrs = adapter.device_addresses().await?;
    for addr in addrs {
        let device = adapter.device(addr)?;
        if device.is_connected().await.unwrap_or(false)
            && let Ok(uuids) = device.uuids().await
            && let Some(uuids) = uuids
            && uuids.iter().any(|u| *u == target_uuid)
        {
            return Ok(device);
        }
    }
    Err(bluer::Error::from(Error::new(
        std::io::ErrorKind::NotFound,
        "No connected AirPods found",
    )))
}

pub async fn find_other_managed_devices(
    adapter: &Adapter,
    managed_macs: Vec<String>,
) -> bluer::Result<Vec<bluer::Device>> {
    let addrs = adapter.device_addresses().await?;
    let mut devices = Vec::new();
    for addr in addrs {
        let device = adapter.device(addr)?;
        let device_mac = device.address().to_string();
        let connected = device.is_connected().await.unwrap_or(false);
        debug!("Checking device: {}, connected: {}", device_mac, connected);
        if connected && managed_macs.contains(&device_mac) {
            debug!("Found managed device: {}", device_mac);
            devices.push(device);
        }
    }
    if !devices.is_empty() {
        return Ok(devices);
    }
    debug!("No other managed devices found");
    Err(bluer::Error::from(Error::new(
        std::io::ErrorKind::NotFound,
        "No other managed devices found",
    )))
}

/// Whether BlueZ currently reports the device with this address as connected.
/// None if that can't be determined (BlueZ unreachable, device unknown).
/// Blocking: call from spawn_blocking.
pub fn device_connected_blocking(mac: &str) -> Option<bool> {
    use dbus::arg::{RefArg, Variant};
    use dbus::blocking::Connection;
    use std::collections::HashMap;
    type Props = HashMap<String, Variant<Box<dyn RefArg>>>;
    type Objects = HashMap<dbus::Path<'static>, HashMap<String, Props>>;

    let conn = Connection::new_system().ok()?;
    let proxy = conn.with_proxy("org.bluez", "/", std::time::Duration::from_secs(5));
    let (objects,): (Objects,) = proxy
        .method_call("org.freedesktop.DBus.ObjectManager", "GetManagedObjects", ())
        .ok()?;
    let mut found = None;
    for interfaces in objects.values() {
        let Some(device) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        let address = device.get("Address").and_then(|a| a.0.as_str());
        if !address.is_some_and(|a| a.eq_ignore_ascii_case(mac)) {
            continue;
        }
        let connected = device
            .get("Connected")
            .and_then(|c| c.0.as_u64())
            .map(|c| c != 0);
        // The same device can be listed under more than one adapter.
        if connected == Some(true) {
            return Some(true);
        }
        found = found.or(connected);
    }
    found
}

/// Async wrapper around device_connected_blocking.
pub async fn device_connected(mac: &str) -> Option<bool> {
    let mac = mac.to_string();
    tokio::task::spawn_blocking(move || device_connected_blocking(&mac))
        .await
        .ok()
        .flatten()
}

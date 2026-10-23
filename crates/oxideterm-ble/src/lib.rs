//! Bluetooth Low Energy discovery and device inspection backed by btleplug.
//!
//! Adapter enumeration, device scanning, and temporary GATT inspection
//! connections are the supported operations; the streaming terminal path is
//! future work. Callers run these functions on a tokio runtime because
//! btleplug is async end to end.

use std::time::{Duration, Instant};

use btleplug::api::{AddressType, Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Manager, PeripheralId};
use thiserror::Error;
use tokio::time::sleep;
use uuid::Uuid;

/// Re-exported so UI layers can store device identity without depending on btleplug.
pub type BlePeripheralId = PeripheralId;

/// Fixed discovery window; BLE scanning is inherently time-boxed.
pub const BLE_SCAN_DURATION: Duration = Duration::from_secs(5);

/// Nordic UART Service (NUS) advertisement UUID; the de facto serial-over-BLE
/// protocol with one writable and one notifiable characteristic.
pub const NORDIC_UART_SERVICE_UUID: Uuid = Uuid::from_u128(0x6e400001_b5a3_f393_e0a9_e50e24dcca9e);

#[derive(Debug, Error)]
pub enum BleError {
    #[error("no bluetooth adapters are available")]
    NoAdapters,
    #[error("bluetooth adapter index {index} is out of range")]
    AdapterOutOfRange { index: usize },
    #[error("connecting to the device timed out")]
    ConnectTimedOut,
    #[error("device is no longer available")]
    DeviceUnavailable,
    #[error(transparent)]
    Platform(#[from] btleplug::Error),
}

#[derive(Clone, Debug)]
pub struct BleAdapterInfo {
    pub name: String,
    /// The controller address for this adapter. macOS (CoreBluetooth) does not
    /// expose a controller MAC, so this is `None` there. Used by profiles to
    /// prefer a specific adapter when multiple are present; the caller falls
    /// back to the first adapter when this is absent or unmatched.
    pub adapter_mac: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BleDeviceInfo {
    pub id: BlePeripheralId,
    pub name: Option<String>,
    pub address: String,
    pub rssi: Option<i16>,
    /// True when the advertisement includes NUS, marking devices that speak
    /// the de facto serial-over-BLE protocol.
    pub supports_nus: bool,
    /// Services advertised in the broadcast packets. This is not the full GATT
    /// service table — only UUIDs the device chose to advertise. Use it as an
    /// optimization hint to skip `discover_services` when the profile's target
    /// UUID is already present; fall back to a real discovery otherwise.
    pub advertised_services: Vec<Uuid>,
    /// Whether the device uses a public/static or resolvable/random address.
    /// Profiles store `device_mac` only for public-address devices because
    /// resolvable private addresses rotate every ~15 minutes.
    pub address_type: Option<AddressType>,
}

/// Enumerates local central adapters in platform order.
pub async fn ble_list_adapters() -> Result<Vec<BleAdapterInfo>, BleError> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let mut infos = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        let info = adapter.adapter_info().await?;
        let adapter_mac = adapter.adapter_address().await?.map(|bd| bd.to_string());
        infos.push(BleAdapterInfo {
            name: adapter_display_name(&info).to_string(),
            adapter_mac,
        });
    }
    Ok(infos)
}

/// Strips the BlueZ modalias suffix btleplug appends to adapter names, e.g.
/// "hci0 (usb:v1D6Bp0246d0555)" becomes "hci0"; the hciN prefix is what users
/// see in bluetoothctl, while the modalias is raw enumeration data. Names
/// without a modalias-typed parenthetical (other platforms) pass through.
fn adapter_display_name(info: &str) -> &str {
    for modalias_prefix in [" (usb:", " (pci:"] {
        if let Some((name, _)) = info.split_once(modalias_prefix) {
            return name;
        }
    }
    info
}

/// Holds a single btleplug [`Adapter`] open across scan, inspection, and
/// connect so peripherals discovered during a scan stay resolvable without
/// re-scanning.
///
/// BlueZ (Linux) reaps unpaired temporary device objects once no discovery
/// session is active, and btleplug caches peripherals per [`Adapter`] on
/// macOS and Windows. Reusing one [`Adapter`] for the whole connection flow
/// avoids both problems: the scan's discovery window stays open (Linux) and
/// its peripheral cache stays populated (macOS/Windows) until the terminal
/// link is up, at which point [`BleAdapterGuard::stop_scan`] releases the
/// radio.
pub struct BleAdapterGuard {
    adapter: btleplug::platform::Adapter,
}

impl BleAdapterGuard {
    /// Opens the adapter at `adapter_index`; discovery is not started until
    /// [`BleAdapterGuard::scan`] is called.
    pub async fn open(adapter_index: usize) -> Result<Self, BleError> {
        let manager = Manager::new().await?;
        let adapter = manager
            .adapters()
            .await?
            .into_iter()
            .nth(adapter_index)
            .ok_or(BleError::AdapterOutOfRange {
                index: adapter_index,
            })?;
        Ok(Self { adapter })
    }

    /// The adapter the guard owns, shared with inspect and connect callers.
    pub fn adapter(&self) -> &btleplug::platform::Adapter {
        &self.adapter
    }

    /// Runs a discovery window and leaves it running so discovered peripherals
    /// remain addressable while the user selects a device and service.
    pub async fn scan(&self, duration: Duration) -> Result<Vec<BleDeviceInfo>, BleError> {
        self.adapter.start_scan(ScanFilter::default()).await?;
        sleep(duration).await;
        let peripherals = match self.adapter.peripherals().await {
            Ok(peripherals) => peripherals,
            Err(error) => {
                let _ = self.adapter.stop_scan().await;
                return Err(error.into());
            }
        };
        let mut devices = collect_discovered_devices(peripherals).await;
        devices.sort_by(device_sort_order);
        // Discovery stays open: the guard owns it and closes it with
        // [`BleAdapterGuard::stop_scan`] once the terminal link is up.
        Ok(devices)
    }

    /// Reads the peripherals the guard's open discovery window has already
    /// collected, without starting a new scan. Callers that received the guard
    /// from an earlier scan (e.g. the connection form) must use this instead
    /// of [`BleAdapterGuard::scan`]: BlueZ rejects a second `start_scan` while
    /// one is active, and restarting discovery would also let BlueZ reap the
    /// temporary device objects the caller's selection refers to.
    pub async fn discovered_devices(&self) -> Result<Vec<BleDeviceInfo>, BleError> {
        let peripherals = self.adapter.peripherals().await?;
        let mut devices = collect_discovered_devices(peripherals).await;
        devices.sort_by(device_sort_order);
        Ok(devices)
    }

    /// Stops the discovery window the guard started. Safe to call when no
    /// scan is running.
    pub async fn stop_scan(&self) {
        let _ = self.adapter.stop_scan().await;
    }
}

/// Reads properties from discovered peripherals into the UI-facing record
/// format, skipping devices whose properties are no longer readable.
async fn collect_discovered_devices(
    peripherals: Vec<btleplug::platform::Peripheral>,
) -> Vec<BleDeviceInfo> {
    let mut devices = Vec::with_capacity(peripherals.len());
    for peripheral in peripherals {
        // Read properties while discovery is still running: BlueZ removes
        // unpaired temporary devices around stop_scan, and querying such a
        // removed /org/bluez/.../dev_XX path fails with a Properties
        // UnknownMethod error that would otherwise abort the whole scan.
        // A device vanishing mid-enumeration is skipped instead of failing
        // the scan because it is no longer addressable anyway.
        // Anonymous peripherals still carry an address, so they stay listed
        // with a name fallback instead of being dropped.
        let Ok(Some(properties)) = peripheral.properties().await else {
            continue;
        };
        devices.push(BleDeviceInfo {
            id: peripheral.id(),
            name: properties.local_name,
            address: properties.address.to_string(),
            rssi: properties.rssi,
            supports_nus: advertises_nus(&properties.services),
            advertised_services: properties.services.clone(),
            address_type: properties.address_type,
        });
    }
    devices
}

/// Orders NUS-capable devices first, then named devices, then by address so
/// the most useful selections surface at the top of the list.
fn device_sort_order(a: &BleDeviceInfo, b: &BleDeviceInfo) -> std::cmp::Ordering {
    b.supports_nus
        .cmp(&a.supports_nus)
        .then_with(|| a.name.is_none().cmp(&b.name.is_none()))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.address.cmp(&b.address))
}

/// Matches the Nordic UART Service UUID against the advertised service list.
fn advertises_nus(services: &[Uuid]) -> bool {
    services.contains(&NORDIC_UART_SERVICE_UUID)
}

/// Upper bound for the inspection link; BlueZ has no connect timeout contract,
/// so an absent device must not park the inspection task indefinitely.
const BLE_INSPECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// GATT layout snapshot of one device, taken over a temporary inspection
/// connection that is closed again before returning.
#[derive(Clone, Debug)]
pub struct BleDeviceDetails {
    pub name: Option<String>,
    pub address: String,
    /// True when the link predated the inspection, so the caller knows the
    /// device was already connected elsewhere and was not disconnected here.
    pub was_connected: bool,
    pub services: Vec<BleServiceInfo>,
}

#[derive(Clone, Debug)]
pub struct BleServiceInfo {
    pub uuid: Uuid,
    pub primary: bool,
    pub characteristics: Vec<BleCharacteristicInfo>,
}

#[derive(Clone, Debug)]
pub struct BleCharacteristicInfo {
    pub uuid: Uuid,
    /// Canonical ATT property names in spec order, e.g. `read`, `notify`;
    /// protocol terms kept untranslated like the dBm unit.
    pub properties: Vec<&'static str>,
}

/// How long a reopened discovery window is given to make bluetoothd recreate
/// a reaped device object before the lookup is reported as failed.
const BLE_REDISCOVER_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval while waiting for the recreated object to appear.
const BLE_REDISCOVER_POLL: Duration = Duration::from_millis(250);

/// Builds a link to a device that an earlier scan listed, reopening discovery
/// first when the adapter no longer has a usable handle to it.
///
/// On Linux the BlueZ device object may have been reaped, and on macOS or
/// Windows the peripheral may be absent from this adapter's cache. Reopening
/// discovery makes the device available again. The discovery window's
/// lifetime is owned by the caller's [`BleAdapterGuard`], so this function
/// does not stop it; closing the window too early would reap the object again
/// before the link is up.
///
/// The second value reports whether the link predated this call, letting the
/// caller leave a connection owned elsewhere untouched.
pub async fn ble_connect_discovered_device(
    adapter: &btleplug::platform::Adapter,
    id: &BlePeripheralId,
    connect_timeout: Duration,
) -> Result<(btleplug::platform::Peripheral, bool), BleError> {
    match adapter.peripheral(id).await {
        Ok(peripheral) => {
            let was_connected = peripheral.is_connected().await?;
            connect_if_needed(&peripheral, was_connected, connect_timeout).await?;
            Ok((peripheral, was_connected))
        }
        Err(_) => {
            adapter.start_scan(ScanFilter::default()).await?;
            match wait_for_recreated_peripheral(adapter, id).await {
                Some(peripheral) => {
                    connect_if_needed(&peripheral, false, connect_timeout).await?;
                    Ok((peripheral, false))
                }
                None => Err(BleError::DeviceUnavailable),
            }
        }
    }
}

/// Connects only when the link does not already exist; a connection owned by
/// another consumer survives instead of being replaced.
async fn connect_if_needed(
    peripheral: &btleplug::platform::Peripheral,
    was_connected: bool,
    connect_timeout: Duration,
) -> Result<(), BleError> {
    if was_connected {
        return Ok(());
    }
    tokio::time::timeout(connect_timeout, peripheral.connect())
        .await
        .map_err(|_| BleError::ConnectTimedOut)??;
    Ok(())
}

/// Connects to one discovered device just long enough to read its GATT
/// layout, then disconnects unless the link already existed.
///
/// `adapter` must be the same one used by the scan that listed `id`; reusing
/// it keeps the BlueZ object (Linux) or the peripheral cache (macOS/Windows)
/// alive without re-scanning.
pub async fn ble_inspect_device(
    adapter: &btleplug::platform::Adapter,
    id: BlePeripheralId,
) -> Result<BleDeviceDetails, BleError> {
    let (peripheral, was_connected) =
        ble_connect_discovered_device(adapter, &id, BLE_INSPECT_CONNECT_TIMEOUT).await?;
    let inspect_result = inspect_connected(&peripheral, was_connected).await;
    // The inspection link is temporary; a link this call did not create belongs
    // to another consumer and must survive the inspection.
    if !was_connected {
        let _ = peripheral.disconnect().await;
    }
    inspect_result
}

/// Waits for bluetoothd to recreate a device object that was reaped when the
/// scanning window closed.
async fn wait_for_recreated_peripheral(
    adapter: &btleplug::platform::Adapter,
    id: &BlePeripheralId,
) -> Option<btleplug::platform::Peripheral> {
    let deadline = Instant::now() + BLE_REDISCOVER_TIMEOUT;
    loop {
        sleep(BLE_REDISCOVER_POLL).await;
        if let Ok(peripheral) = adapter.peripheral(id).await {
            return Some(peripheral);
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// Reads the GATT layout over a link the caller already established.
async fn inspect_connected(
    peripheral: &btleplug::platform::Peripheral,
    was_connected: bool,
) -> Result<BleDeviceDetails, BleError> {
    peripheral.discover_services().await?;
    let properties = peripheral
        .properties()
        .await?
        .ok_or(BleError::DeviceUnavailable)?;
    let services = peripheral
        .services()
        .iter()
        .map(|service| BleServiceInfo {
            uuid: service.uuid,
            primary: service.primary,
            characteristics: service
                .characteristics
                .iter()
                .map(|characteristic| BleCharacteristicInfo {
                    uuid: characteristic.uuid,
                    properties: characteristic_property_names(characteristic.properties),
                })
                .collect(),
        })
        .collect();
    Ok(BleDeviceDetails {
        name: properties.local_name,
        address: properties.address.to_string(),
        was_connected,
        services,
    })
}

/// Maps ATT characteristic property bits to canonical spec names in spec
/// declaration order.
fn characteristic_property_names(flags: btleplug::api::CharPropFlags) -> Vec<&'static str> {
    use btleplug::api::CharPropFlags as Flags;
    let mut names = Vec::new();
    if flags.contains(Flags::BROADCAST) {
        names.push("broadcast");
    }
    if flags.contains(Flags::READ) {
        names.push("read");
    }
    if flags.contains(Flags::WRITE_WITHOUT_RESPONSE) {
        names.push("write-without-response");
    }
    if flags.contains(Flags::WRITE) {
        names.push("write");
    }
    if flags.contains(Flags::NOTIFY) {
        names.push("notify");
    }
    if flags.contains(Flags::INDICATE) {
        names.push("indicate");
    }
    if flags.contains(Flags::AUTHENTICATED_SIGNED_WRITES) {
        names.push("authenticated-signed-writes");
    }
    if flags.contains(Flags::EXTENDED_PROPERTIES) {
        names.push("extended-properties");
    }
    names
}

/// Returns a human-readable short name for Bluetooth SIG 16-bit service UUIDs;
/// non-16-bit UUIDs (including vendor services like NUS) fall back to their
/// full canonical form.
pub fn service_short_name(uuid: &Uuid) -> String {
    let Some(value) = extract_16bit_uuid(uuid) else {
        return uuid.to_string();
    };
    let name = match value {
        0x1800 => "Generic Access",
        0x1801 => "Generic Attribute",
        0x1802 => "Immediate Alert",
        0x1803 => "Link Loss",
        0x1804 => "Tx Power",
        0x1805 => "Current Time",
        0x1806 => "Reference Time Update",
        0x1807 => "Next DST Change",
        0x1808 => "Glucose",
        0x1809 => "Health Thermometer",
        0x180A => "Device Information",
        0x180D => "Heart Rate",
        0x180E => "Phone Alert Status",
        0x180F => "Battery Service",
        0x1810 => "Blood Pressure",
        0x1811 => "Alert Notification",
        0x1812 => "HID",
        0x1813 => "Scan Parameters",
        0x1814 => "Running Speed and Cadence",
        0x1815 => "Automation IO",
        0x1816 => "Cycling Speed and Cadence",
        0x1818 => "Cycling Power",
        0x1819 => "Location and Navigation",
        0x181A => "Environmental Sensing",
        0x181B => "Body Composition",
        0x181C => "User Data",
        0x181D => "Weight Scale",
        0x181E => "Bond Management",
        0x181F => "Continuous Glucose Monitoring",
        0x1820 => "Internet Protocol Support",
        0x1821 => "Indoor Positioning",
        0x1822 => "Pulse Oximeter",
        0x1823 => "HTTP Proxy",
        0x1824 => "Transport Discovery",
        0x1825 => "Object Transfer",
        0x1826 => "Fitness Machine",
        0x1827 => "Mesh Provisioning",
        0x1828 => "Mesh Proxy",
        0x1829 => "Reconnection Configuration",
        0x182A => "Insulin Delivery",
        0x182B => "Binary Sensor",
        0x182C => "Emergency Configuration",
        0x182D => "Physical Activity Monitor",
        0x182E => "Elapsed Time",
        0x182F => "Generic Health Sensor",
        0x1830 => "Audio Input Control",
        0x1831 => "Volume Control",
        0x1832 => "Audio Stream Control",
        0x1833 => "Bearing Information",
        0x1834 => "Constant Tone Extension",
        0x1835 => "Telephone Bearer",
        0x1836 => "Generic Telephone Bearer",
        0x1837 => "Microphone Control",
        0x1838 => "Audio Stream Content Control",
        0x1843 => "Coordinated Set Identification",
        0x1844 => "Device Time",
        0x1845 => "Constant Tone Extension Sync",
        0x1848 => "Published Audio Capabilities",
        0x1849 => "Basic Audio Announcement",
        0x184A => "Broadcast Audio Announcement",
        0x184B => "Common Audio",
        0x184C => "Hearing Access",
        0x184D => "Telephony and Media Audio",
        0x184E => "Public Broadcast Announcement",
        0x184F => "Electronic Shelf Label",
        0x1850 => "Gaming Audio",
        0x1851 => "Mesh Proxy Solicitation",
        _ => return format!("0x{value:04X}"),
    };
    name.to_string()
}

/// A 16-bit Bluetooth UUID is the 32-bit identifier `0x0000XXXX` embedded in
/// the Bluetooth base UUID `00000000-0000-1000-8000-00805F9B34FB`.
fn extract_16bit_uuid(uuid: &Uuid) -> Option<u16> {
    let bytes = uuid.as_bytes();
    let mut base = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34,
        0xFB,
    ];
    base[2] = bytes[2];
    base[3] = bytes[3];
    if bytes == &base {
        Some(u16::from_be_bytes([bytes[2], bytes[3]]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NORDIC_UART_SERVICE_UUID, adapter_display_name, advertises_nus,
        characteristic_property_names,
    };
    use btleplug::api::CharPropFlags;
    use uuid::Uuid;

    #[test]
    fn adapter_display_name_strips_modalias_but_keeps_other_parentheticals() {
        assert_eq!(adapter_display_name("hci0 (usb:v1D6Bp0246d0555)"), "hci0");
        assert_eq!(
            adapter_display_name("hci0 (pci:v00008086d00000084sv00008086sd0010bcE0sc01sc01)"),
            "hci0"
        );
        assert_eq!(adapter_display_name("hci1"), "hci1");
        // Names without a modalias-typed parenthetical must survive untouched,
        // so the strip stays limited to the BlueZ enumeration format.
        assert_eq!(
            adapter_display_name("Generic Adapter (internal)"),
            "Generic Adapter (internal)"
        );
    }

    #[test]
    fn advertises_nus_matches_only_the_nordic_uart_service_uuid() {
        // The canonical string form pins the constant against u128 typos.
        let nus = Uuid::parse_str("6e400001-b5a3-f393-e0a9-e50e24dcca9e").unwrap();
        assert_eq!(nus, NORDIC_UART_SERVICE_UUID);
        assert!(advertises_nus(&[nus]));
        assert!(!advertises_nus(&[]));
        assert!(!advertises_nus(&[Uuid::from_u128(
            0x6e400002_b5a3_f393_e0a9_e50e24dcca9e
        )]));
    }

    #[test]
    fn characteristic_property_names_follow_spec_order() {
        // write-without-response must stay ahead of write so the rendered
        // property list is stable and matches the ATT declaration order.
        let combined = CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE;
        assert_eq!(
            characteristic_property_names(combined),
            vec!["write-without-response", "write"]
        );
        assert_eq!(
            characteristic_property_names(CharPropFlags::READ | CharPropFlags::NOTIFY),
            vec!["read", "notify"]
        );
        assert!(characteristic_property_names(CharPropFlags::empty()).is_empty());
    }

    #[test]
    fn service_short_name_resolves_sig_16bit_uuids_and_falls_back_for_vendor() {
        let battery = Uuid::parse_str("0000180f-0000-1000-8000-00805f9b34fb").unwrap();
        assert_eq!(super::service_short_name(&battery), "Battery Service");
        // Unmapped 16-bit UUIDs keep their numeric identifier so the user can
        // still identify the service without the long form.
        let unknown = Uuid::parse_str("0000fb00-0000-1000-8000-00805f9b34fb").unwrap();
        assert_eq!(super::service_short_name(&unknown), "0xFB00");
        // Vendor UUIDs (e.g. NUS) do not match the Bluetooth base and must
        // surface their full canonical form.
        assert_eq!(
            super::service_short_name(&NORDIC_UART_SERVICE_UUID),
            "6e400001-b5a3-f393-e0a9-e50e24dcca9e"
        );
    }
}

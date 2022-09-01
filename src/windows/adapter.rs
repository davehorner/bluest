use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::future::ready;
use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use tracing::{debug, error, trace, warn};
use windows::core::{InParam, HSTRING};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEAdvertisementWatcherStoppedEventArgs, BluetoothLEManufacturerData,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind};
use windows::Devices::Radios::{Radio, RadioState};
use windows::Foundation::Collections::{IIterable, IVector};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataReader;

use super::device::{Device, DeviceId};
use super::types::StringVec;
use crate::error::{Error, ErrorKind};
use crate::util::defer;
use crate::{AdapterEvent, AdvertisementData, AdvertisingDevice, BluetoothUuidExt, ManufacturerData, Result, Uuid};

/// The system's Bluetooth adapter interface.
///
/// The default adapter for the system may be created with the [`Adapter::default()`] method.
#[derive(Clone)]
pub struct Adapter {
    inner: BluetoothAdapter,
}

impl PartialEq for Adapter {
    fn eq(&self, other: &Self) -> bool {
        self.inner.DeviceId() == other.inner.DeviceId()
    }
}

impl Eq for Adapter {}

impl std::hash::Hash for Adapter {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.DeviceId().unwrap().to_os_string().hash(state);
    }
}

impl std::fmt::Debug for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Adapter").field(&self.inner.DeviceId().unwrap()).finish()
    }
}

impl Adapter {
    /// Creates the default adapter for the system
    pub async fn default() -> Option<Self> {
        let adapter = BluetoothAdapter::GetDefaultAsync().ok()?.await.ok()?;
        Some(Adapter { inner: adapter })
    }

    /// A stream of [`AdapterEvent`] which allows the application to identify when the adapter is enabled or disabled.
    pub async fn events(&self) -> Result<impl Stream<Item = Result<AdapterEvent>> + '_> {
        let (mut sender, receiver) = futures_channel::mpsc::channel(16);
        let radio = self.inner.GetRadioAsync()?.await?;
        let token = radio.StateChanged(&TypedEventHandler::new(move |radio: &Option<Radio>, _| {
            let radio = radio.as_ref().expect("radio is null in StateChanged event");
            let state = radio.State().expect("radio state getter failed in StateChanged event");
            let res = sender.try_send(if state == RadioState::On {
                Ok(AdapterEvent::Available)
            } else {
                Ok(AdapterEvent::Unavailable)
            });

            if let Err(err) = res {
                error!("Unable to send AdapterEvent: {:?}", err);
            }

            Ok(())
        }))?;

        let guard = defer(move || {
            if let Err(err) = radio.RemoveStateChanged(token) {
                error!("Error removing state changed handler: {:?}", err);
            }
        });

        Ok(receiver.map(move |x| {
            let _guard = &guard;
            x
        }))
    }

    /// Asynchronously blocks until the adapter is available
    pub async fn wait_available(&self) -> Result<()> {
        let radio = self.inner.GetRadioAsync()?.await?;
        let events = self.events().await?;
        let state = radio.State()?;
        if state != RadioState::On {
            events
                .skip_while(|x| ready(x.is_ok() && !matches!(x, Ok(AdapterEvent::Available))))
                .next()
                .await
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Internal,
                        None,
                        "adapter event stream closed unexpectedly".to_string(),
                    )
                })??;
        }
        Ok(())
    }

    /// Attempts to create the device identified by `id`
    pub async fn open_device(&self, id: &DeviceId) -> Result<Device> {
        Device::from_id(&id.0.as_os_str().into()).await.map_err(Into::into)
    }

    /// Finds all connected Bluetooth LE devices
    pub async fn connected_devices(&self) -> Result<Vec<Device>> {
        let aqsfilter = BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)?;

        let op = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &aqsfilter,
            InParam::null(),
            DeviceInformationKind::AssociationEndpoint,
        )?;
        let devices = op.await?;
        let device_ids: Vec<HSTRING> = devices
            .into_iter()
            .map(|x| x.Id())
            .collect::<windows::core::Result<_>>()?;

        let mut res = Vec::with_capacity(device_ids.len());
        for id in device_ids {
            res.push(Device::from_id(&id).await?);
        }

        Ok(res)
    }

    /// Finds all connected devices providing any service in `services`
    ///
    /// # Panics
    ///
    /// Panics if `services` is empty.
    pub async fn connected_devices_with_services(&self, services: &[Uuid]) -> Result<Vec<Device>> {
        assert!(!services.is_empty());

        // Find all connected devices
        let aqsfilter = BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)?;

        debug!("aqs filter = {:?}", aqsfilter);

        let op = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &aqsfilter,
            InParam::null(),
            DeviceInformationKind::AssociationEndpoint,
        )?;
        let devices = op.await?;

        trace!("found {} connected devices", devices.Size()?);

        if devices.Size()? == 0 {
            return Ok(Vec::new());
        }

        // Build an AQS filter for services of any of the connected devices
        let mut devicefilter = OsString::new();
        for device in devices {
            if !devicefilter.is_empty() {
                devicefilter.push(" OR ");
            }
            devicefilter.push("System.Devices.AepService.AepId:=\"");
            devicefilter.push(device.Id()?.to_os_string());
            devicefilter.push("\"");
        }

        debug!("device filter = {:?}", devicefilter);

        // Build an AQS filter for any of the service Uuids
        let mut servicefilter = String::new();
        for service in services {
            if !servicefilter.is_empty() {
                servicefilter.push_str(" OR ");
            }
            servicefilter.push_str("System.Devices.AepService.Bluetooth.ServiceGuid:=\"{");
            servicefilter.push_str(&service.to_string());
            servicefilter.push_str("}\"");
        }

        debug!("service filter = {:?}", servicefilter);

        // Combine the device and service filters
        let mut aqsfilter =
            OsString::from("System.Devices.AepService.ProtocolId:=\"{BB7BB05E-5972-42B5-94FC-76EAA7084D49}\" AND (");
        aqsfilter.push(devicefilter);
        aqsfilter.push(") AND (");
        aqsfilter.push(servicefilter);
        aqsfilter.push(")");
        let aqsfilter: HSTRING = aqsfilter.into();

        debug!("aqs filter = {:?}", aqsfilter);

        // Find all associated endpoint services matching the filter
        let aep_id = HSTRING::from("System.Devices.AepService.AepId");
        let additional_properties = StringVec::new(vec![aep_id.clone()]);
        let op = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &aqsfilter,
            &IIterable::from(additional_properties),
            DeviceInformationKind::AssociationEndpointService,
        )?;
        let services = op.await?;

        trace!("found {} matching services of connected devices", services.Size()?);

        // Find the unique set of device ids which matched
        let mut device_ids = HashSet::with_capacity(services.Size()? as usize);
        for service in services {
            let id = service.Properties()?.Lookup(&aep_id)?;
            let id: HSTRING = id.try_into()?;
            device_ids.insert(id.to_os_string());
        }

        trace!("found {} devices with at least one matching service", device_ids.len());

        // Build the devices
        let mut res = Vec::with_capacity(device_ids.len());
        for id in device_ids {
            res.push(Device::from_id(&id.into()).await?);
        }

        Ok(res)
    }

    /// Starts scanning for Bluetooth advertising packets.
    ///
    /// Returns a stream of [`AdvertisingDevice`] structs which contain the data from the advertising packet and the
    /// [`Device`] which sent it. Scanning is automatically stopped when the stream is dropped. Inclusion of duplicate
    /// packets is a platform-specific implementation detail.
    ///
    /// If `services` is not empty, returns advertisements including at least one GATT service with a UUID in
    /// `services`. Otherwise returns all advertisements.
    pub async fn scan<'a>(&'a self, services: &'a [Uuid]) -> Result<impl Stream<Item = AdvertisingDevice> + 'a> {
        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.SetAllowExtendedAdvertisements(true)?;

        let (sender, receiver) = futures_channel::mpsc::channel(16);
        let sender = Arc::new(std::sync::Mutex::new(sender));

        let weak_sender = Arc::downgrade(&sender);
        watcher.Received(&TypedEventHandler::new(
            move |_watcher, event_args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Some(sender) = weak_sender.upgrade() {
                    if let Some(event_args) = event_args {
                        let res = sender.lock().unwrap().try_send(event_args.clone());
                        if let Err(err) = res {
                            error!("Unable to send AdvertisingDevice: {:?}", err);
                        }
                    }
                }
                Ok(())
            },
        ))?;

        let mut sender = Some(sender);
        watcher.Stopped(&TypedEventHandler::new(
            move |_watcher, _event_args: &Option<BluetoothLEAdvertisementWatcherStoppedEventArgs>| {
                // Drop the sender, ending the stream
                let _sender = sender.take();
                Ok(())
            },
        ))?;

        watcher.Start()?;
        let guard = defer(move || {
            if let Err(err) = watcher.Stop() {
                error!("Error stopping scan: {:?}", err);
            }
        });

        Ok(receiver
            .map(|event_args| -> windows::core::Result<_> {
                // Parse relevant fields from event_args
                let addr = (event_args.BluetoothAddress()?, event_args.BluetoothAddressType()?);
                let rssi = event_args.RawSignalStrengthInDBm().ok();
                let adv_data = AdvertisementData::from(event_args);
                Ok((addr, rssi, adv_data))
            })
            .filter_map(move |res| {
                let _guard = &guard;

                // Filter by result and services
                ready(match res {
                    Ok((addr, rssi, adv_data)) => (services.is_empty()
                        || services.iter().any(|x| adv_data.services.contains(x)))
                    .then(|| (addr, rssi, adv_data)),
                    Err(err) => {
                        warn!("Error getting bluetooth address from event: {:?}", err);
                        None
                    }
                })
            })
            .then(|(addr, rssi, adv_data)| {
                // Create the Device
                Box::pin(async move {
                    Device::from_addr(addr.0, addr.1)
                        .await
                        .map(|device| AdvertisingDevice { device, rssi, adv_data })
                })
            })
            .filter_map(move |res| {
                ready(match res {
                    Ok(dev) => Some(dev),
                    Err(err) => {
                        warn!("Error creating device: {:?}", err);
                        None
                    }
                })
            }))
    }

    /// Finds Bluetooth devices providing any service in `services`.
    ///
    /// Returns a stream of [`Device`] structs with matching connected devices returned first. If the stream is not
    /// dropped before all matching connected devices are consumed then scanning will begin for devices advertising any
    /// of the `services`. Scanning will continue until the stream is dropped. Inclusion of duplicate devices is a
    /// platform-specific implementation detail.
    pub async fn discover_devices<'a>(
        &'a self,
        services: &'a [Uuid],
    ) -> Result<impl Stream<Item = Result<Device>> + 'a> {
        use futures_util::TryFutureExt;

        let connected = self.connected_devices_with_services(services).await?;
        let advertising = Box::pin(async {
            match self.scan(services).await {
                Ok(stream) => Ok(stream.map(|x| Ok(x.device))),
                Err(err) => Err(err),
            }
        })
        .try_flatten_stream();

        Ok(futures_util::stream::iter(connected).map(Ok).chain(advertising))
    }

    /// Connects to the [`Device`]
    pub async fn connect_device(&self, device: &Device) -> Result<()> {
        device.connect().await
    }

    /// Disconnects from [`Device`]
    pub async fn disconnect_device(&self, device: &Device) -> Result<()> {
        device.disconnect().await
    }
}

impl TryFrom<BluetoothLEManufacturerData> for ManufacturerData {
    type Error = windows::core::Error;

    fn try_from(val: BluetoothLEManufacturerData) -> Result<Self, Self::Error> {
        let company_id = val.CompanyId()?;
        let buf = val.Data()?;
        let mut data = vec![0; buf.Length()? as usize];
        let reader = DataReader::FromBuffer(&buf)?;
        reader.ReadBytes(data.as_mut_slice())?;
        Ok(ManufacturerData { company_id, data })
    }
}

impl From<BluetoothLEAdvertisementReceivedEventArgs> for AdvertisementData {
    fn from(event_args: BluetoothLEAdvertisementReceivedEventArgs) -> Self {
        let is_connectable = event_args.IsConnectable().unwrap_or(false);
        let tx_power_level = event_args.TransmitPowerLevelInDBm().ok().and_then(|x| x.Value().ok());
        let (local_name, manufacturer_data, services, service_data) = if let Ok(adv) = event_args.Advertisement() {
            let local_name = adv
                .LocalName()
                .ok()
                .and_then(|x| (!x.is_empty()).then(|| x.to_string_lossy()));
            let manufacturer_data = adv
                .ManufacturerData()
                .and_then(|x| x.GetAt(0))
                .and_then(|x| x.try_into())
                .ok();

            let services = adv
                .ServiceUuids()
                .map(|x| x.into_iter().map(|x| Uuid::from_u128(x.to_u128())).collect())
                .unwrap_or_default();

            let service_data = if let Ok(data_sections) = adv.DataSections() {
                to_service_data(&data_sections).unwrap_or_default()
            } else {
                Default::default()
            };

            (local_name, manufacturer_data, services, service_data)
        } else {
            (None, None, Vec::new(), HashMap::new())
        };

        AdvertisementData {
            local_name,
            manufacturer_data,
            services,
            tx_power_level,
            is_connectable,
            service_data,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UuidKind {
    U16,
    U32,
    U128,
}

fn read_uuid(reader: &DataReader, kind: UuidKind) -> windows::core::Result<Uuid> {
    Ok(match kind {
        UuidKind::U16 => Uuid::from_u16(reader.ReadUInt16()?),
        UuidKind::U32 => Uuid::from_u32(reader.ReadUInt32()?),
        UuidKind::U128 => {
            let mut uuid = [0u8; 16];
            reader.ReadBytes(&mut uuid)?;
            Uuid::from_bytes(uuid)
        }
    })
}

fn to_service_data(
    data_sections: &IVector<BluetoothLEAdvertisementDataSection>,
) -> windows::core::Result<HashMap<Uuid, Vec<u8>>> {
    let mut service_data = HashMap::new();

    for data in data_sections {
        let kind = match data.DataType()? {
            0x16 => Some(UuidKind::U16),
            0x20 => Some(UuidKind::U32),
            0x21 => Some(UuidKind::U128),
            _ => None,
        };

        if let Some(kind) = kind {
            let buf = data.Data()?;
            let reader = DataReader::FromBuffer(&buf)?;
            if let Ok(uuid) = read_uuid(&reader, kind) {
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut value = vec![0; len];
                reader.ReadBytes(value.as_mut_slice())?;
                service_data.insert(uuid, value);
            }
        }
    }

    Ok(service_data)
}

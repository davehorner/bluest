#![allow(clippy::let_unit_value)]

use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::Stream;
use objc_foundation::{INSArray, INSFastEnumeration, NSArray};
use objc_id::ShareId;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::debug;
use uuid::Uuid;

use super::delegates::{self, CentralDelegate};
use super::device::Device;
use super::types::{dispatch_queue_create, dispatch_release, nil, CBCentralManager, CBManagerState, CBUUID, NSUUID};

use crate::error::ErrorKind;
use crate::{AdapterEvent, AdvertisementData, AdvertisingDevice, DeviceId, Error, Result};

/// The system's Bluetooth adapter interface.
///
/// The default adapter for the system may be accessed with the [Adapter::default()] method.
#[derive(Clone)]
pub struct Adapter {
    central: ShareId<CBCentralManager>,
    sender: tokio::sync::broadcast::Sender<delegates::CentralEvent>,
    scanning: Arc<AtomicBool>,
}

impl PartialEq for Adapter {
    fn eq(&self, other: &Self) -> bool {
        self.central == other.central
    }
}

impl Eq for Adapter {}

impl std::hash::Hash for Adapter {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.central.hash(state);
    }
}

impl std::fmt::Debug for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Adapter").field(&self.central).finish()
    }
}

impl Adapter {
    /// Creates an interface to the default Bluetooth adapter for the system
    pub async fn default() -> Option<Self> {
        let (sender, _) = tokio::sync::broadcast::channel(16);
        let delegate = CentralDelegate::with_sender(sender.clone())?;
        let central = unsafe {
            let queue = dispatch_queue_create(CStr::from_bytes_with_nul(b"BluetoothQueue\0").unwrap().as_ptr(), nil);
            if queue.is_null() {
                return None;
            }
            let central = CBCentralManager::with_delegate(delegate, queue);
            dispatch_release(queue);
            central.share()
        };

        Some(Adapter {
            central,
            sender,
            scanning: Arc::new(AtomicBool::new(false)),
        })
    }

    /// A stream of [AdapterEvent] which allows the application to identify when the adapter is enabled or disabled.
    pub async fn events(&self) -> Result<impl Stream<Item = Result<AdapterEvent>> + '_> {
        let receiver = self.sender.subscribe();
        Ok(BroadcastStream::new(receiver).filter_map(|x| match x {
            Ok(delegates::CentralEvent::StateChanged) => {
                // TODO: Check CBCentralManager::authorization()?
                let state = self.central.state();
                debug!("Central state is now {:?}", state);
                match state {
                    CBManagerState::PoweredOn => Some(Ok(AdapterEvent::Available)),
                    _ => Some(Ok(AdapterEvent::Unavailable)),
                }
            }
            Err(err) => Some(Err(Error::new(
                ErrorKind::Internal,
                Some(Box::new(err)),
                "adapter event stream".to_string(),
            ))),
            _ => None,
        }))
    }

    /// Asynchronously blocks until the adapter is available
    pub async fn wait_available(&self) -> Result<()> {
        let events = self.events();
        if self.central.state() != CBManagerState::PoweredOn {
            events
                .await?
                .skip_while(|x| x.is_ok() && !matches!(x, Ok(AdapterEvent::Available)))
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
    pub async fn open_device(&self, id: DeviceId) -> Result<Device> {
        let identifiers = NSArray::from_vec(vec![NSUUID::from_uuid(id.0)]);
        let peripherals = self.central.retrieve_peripherals_with_identifiers(identifiers);
        peripherals
            .first_object()
            .map(|x| Device::new(unsafe { ShareId::from_ptr(x as *const _ as *mut _) }))
            .ok_or_else(|| Error::new(ErrorKind::NotFound, None, "opening device".to_string()))
    }

    /// Finds all connected devices providing any service in `services`
    pub async fn connected_devices(&self, services: &[Uuid]) -> Result<Vec<Device>> {
        let services = (!services.is_empty()).then(|| {
            let vec = services.iter().copied().map(CBUUID::from_uuid).collect::<Vec<_>>();
            NSArray::from_vec(vec)
        });
        let peripherals = self.central.retrieve_connected_peripherals_with_services(services);
        Ok(peripherals
            .enumerator()
            .map(|x| Device::new(unsafe { ShareId::from_ptr(x as *const _ as *mut _) }))
            .collect())
    }

    /// Starts scanning for Bluetooth advertising packets.
    ///
    /// Returns a stream of [AdvertisingDevice] structs which contain the data from the advertising packet and the
    /// [Device] which sent it. Scanning is automatically stopped when the stream is dropped. Inclusion of duplicate
    /// packets is a platform-specific implementation detail.
    pub async fn scan<'a>(&'a self, services: &'a [Uuid]) -> Result<impl Stream<Item = AdvertisingDevice> + 'a> {
        if self.central.state() != CBManagerState::PoweredOn {
            Err(ErrorKind::AdapterUnavailable)?
        }

        if self.scanning.swap(true, Ordering::Acquire) {
            Err(ErrorKind::AlreadyScanning)?;
        }

        let services = (!services.is_empty()).then(|| {
            let vec = services.iter().copied().map(CBUUID::from_uuid).collect::<Vec<_>>();
            NSArray::from_vec(vec)
        });

        let guard = scopeguard::guard((), |_| {
            self.central.stop_scan();
            self.scanning.store(false, Ordering::Release);
        });

        let events = BroadcastStream::new(self.sender.subscribe())
            .take_while(|_| self.central.state() == CBManagerState::PoweredOn)
            .filter_map(move |x| {
                let _guard = &guard;
                match x {
                    Ok(delegates::CentralEvent::Discovered {
                        peripheral,
                        adv_data,
                        rssi,
                    }) => Some(AdvertisingDevice {
                        device: Device::new(peripheral),
                        adv_data: AdvertisementData::from_nsdictionary(adv_data),
                        rssi: Some(rssi),
                    }),
                    _ => None,
                }
            });

        self.central.scan_for_peripherals_with_services(services, None);

        Ok(events)
    }

    /// Connects to the [Device]
    pub async fn connect_device(&self, device: &Device) -> Result<()> {
        if self.central.state() != CBManagerState::PoweredOn {
            Err(ErrorKind::AdapterUnavailable)?
        }

        let mut events = BroadcastStream::new(self.sender.subscribe());
        debug!("Connecting to {:?}", device);
        self.central.connect_peripheral(&*device.peripheral, None);
        while let Some(event) = events.next().await {
            if self.central.state() != CBManagerState::PoweredOn {
                Err(ErrorKind::AdapterUnavailable)?
            }
            match event {
                Ok(delegates::CentralEvent::Connect { peripheral }) if peripheral == device.peripheral => break,
                Ok(delegates::CentralEvent::ConnectFailed { peripheral, error }) if peripheral == device.peripheral => {
                    return Err(error
                        .map(Error::from_nserror)
                        .unwrap_or_else(|| ErrorKind::ConnectionFailed.into()));
                }
                _ => (),
            }
        }

        Ok(())
    }

    /// Disconnects from the [Device]
    pub async fn disconnect_device(&self, device: &Device) -> Result<()> {
        if self.central.state() != CBManagerState::PoweredOn {
            Err(ErrorKind::AdapterUnavailable)?
        }

        let mut events = BroadcastStream::new(self.sender.subscribe());
        debug!("Disconnecting from {:?}", device);
        self.central.cancel_peripheral_connection(&*device.peripheral);
        while let Some(event) = events.next().await {
            if self.central.state() != CBManagerState::PoweredOn {
                Err(ErrorKind::AdapterUnavailable)?
            }
            match event {
                Ok(delegates::CentralEvent::Disconnect { peripheral, error }) if peripheral == device.peripheral => {
                    return Err(error
                        .map(Error::from_nserror)
                        .unwrap_or_else(|| ErrorKind::ConnectionFailed.into()));
                }
                _ => (),
            }
        }

        Ok(())
    }
}
use std::time::Duration;
use std::io; // Use std::io::Error as the error type
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::sync::Arc;

use futures_core::Stream;
use futures_lite::{stream, StreamExt};
use tracing::{debug, error, trace, warn};
use windows::core::HSTRING;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisement, BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementFilter,
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementType, BluetoothLEAdvertisementWatcher,
    BluetoothLEAdvertisementWatcherStoppedEventArgs, BluetoothLEManufacturerData, BluetoothLEScanningMode,
    BluetoothLEAdvertisementFlags, BluetoothLEAdvertisementPublisher,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind};
use windows::Devices::Radios::{Radio, RadioState};
use windows::Foundation::Collections::{IIterable, IVector};
use crate::error::{Error, ErrorKind};
use crate::{
    AdapterEvent, AdvertisementData, AdvertisingDevice, AdvertisingGuard, BluetoothUuidExt, ConnectionEvent, Device, DeviceId, ManufacturerData, Result, Uuid
};
use windows::Storage::Streams::DataWriter;

#[derive(Debug, Clone)]
pub struct AdvertisementImpl {
    publisher: Option<BluetoothLEAdvertisementPublisher>,
}

impl AdvertisementImpl {
    /// Creates a new `Advertisement` instance with the specified company ID.
    pub fn new() -> Self {
        Self {
            publisher: None, // Initialize without publisher
        }
    }

    pub async fn advertise(&mut self, data: &Vec<u8>, advertise_duration: Option<Duration>) -> Result<(), io::Error> {

        // Start the publisher if it exists
        if let Some(publisher) = &self.publisher {
            println!("stop on publisher");
            publisher.Stop()?;
            self.publisher=None;
        }

        if self.publisher.is_none() {
            // Initialize BluetoothLEAdvertisement and publisher if not already created
            let manufacturer_data: BluetoothLEManufacturerData = BluetoothLEManufacturerData::new()?;
            // manufacturer_data.SetCompanyId(self.company_id)?;
            // println!("Windows advertisement started with company ID: {:X}.", self.company_id);
            let writer = DataWriter::new()?;
            writer.WriteBytes(data)?;
        
            let buffer = writer.DetachBuffer()?;
            manufacturer_data.SetData(&buffer)?;
            
            let blue = BluetoothLEAdvertisement::new()?;
            // blue.SetFlags(None)?;
            //let manufacturer_data_section = BluetoothLEAdvertisementDataSection::new()?;
          //  manufacturer_data_section.SetData(&buffer)?;
            //blue.DataSections()?.Append(&manufacturer_data_section)?;

            // Create the publisher and start advertising
            //let publisher = BluetoothLEAdvertisementPublisher::Create(&blue)?;
            let publisher = BluetoothLEAdvertisementPublisher::new()?;
            publisher.Advertisement()?.ManufacturerData()?.Append(&manufacturer_data)?;
            //  publisher.Start()?; // Start the publisher before assigning it to `self.publisher`
    
            // Assign the successfully started publisher to `self.publisher`
            self.publisher = Some(publisher);
        } 
        

        if let Some(publisher) = &self.publisher {
            println!("{:?}",publisher.Status());
            publisher.Start()?;
        }

        if let Some(duration) = advertise_duration {
            tokio::time::sleep(duration).await;
            if let Some(publisher) = &self.publisher {
                publisher.Stop()?; // Stop the advertisement
                self.publisher = None; // Clear the publisher to ensure it can be restarted if needed
            }
            println!("Windows advertisement stopped after {:?}", duration);
        }
        Ok(())
    }

    pub fn stop_advertising(&mut self) -> Result<(), io::Error> {
        println!("Windows advertisement manually stopped.");
        if let Some(publisher) = &self.publisher {
            let _ = publisher.Stop()?; // Stop the advertisement
            self.publisher = None; // Clear the publisher to ensure it can be restarted if needed
        }
        Ok(())
    }
    
    pub async fn start_advertising(&mut self, data: AdvertisementData) -> Result<AdvertisingGuard, String> {
        // Create a new Bluetooth advertisement
        let advertisement = BluetoothLEAdvertisement::new().map_err(|e| format!("Failed to create advertisement: {:?}", e))?;
        
        // Set manufacturer data, service UUIDs, etc., from AdvertisementData
        if let Some(manufacturer_data) = data.manufacturer_data {
            let manufacturer_section = BluetoothLEManufacturerData::new().map_err(|e| format!("Failed to create manufacturer data: {:?}", e))?;
            let _ = manufacturer_section.SetCompanyId(manufacturer_data.company_id);
            // Convert Vec<u8> to IBuffer
            let writer = DataWriter::new().map_err(|e| format!("Failed to create DataWriter: {:?}", e))?;
            writer.WriteBytes(&manufacturer_data.data).map_err(|e| format!("Failed to write bytes: {:?}", e))?;
            let buffer = writer.DetachBuffer().map_err(|e| format!("Failed to detach buffer: {:?}", e))?;
            manufacturer_section.SetData(&buffer).map_err(|e| format!("Failed to set data: {:?}", e))?;
            let _manufacturer_data = advertisement
            .ManufacturerData()
            .map_err(|e| format!("Failed to access ManufacturerData: {:?}", e))
            .and_then(|data| {
                data.Append(&manufacturer_section)
                    .map_err(|e| format!("Failed to append manufacturer data: {:?}", e))
            })?;
        
            // Create the publisher and start advertising
            let publisher: BluetoothLEAdvertisementPublisher = BluetoothLEAdvertisementPublisher::Create(&advertisement)
            .map_err(|e| format!("Failed to create publisher: {:?}", e))?;
            publisher
            .Start()
            .map_err(|e| format!("Failed to start advertising: {:?}", e))?;
            return Ok(AdvertisingGuard { advertisement: AdvertisementImpl { publisher: Some(publisher) } });
        }
        Err("no data to send.".to_owned())
    }
}

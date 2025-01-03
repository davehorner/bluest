use core::fmt;
use std::io;

use objc::class;
use objc::rc::StrongPtr;
use objc::runtime::{Class, Object};
use objc::{msg_send, sel, sel_impl};
use tracing::debug;

use crate::{AdvertisementData, AdvertisingGuard, Result};

#[derive(Clone)]
pub struct AdvertisementImpl {
    peripheral_manager: Option<StrongPtr>,
}

impl fmt::Debug for AdvertisementImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AdvertisementImpl {{ peripheral_manager: ... }}")
    }
}

impl AdvertisementImpl {
    pub fn new() -> Self {
        Self {
            peripheral_manager: None,
        }
    }

    pub fn stop_advertising(&mut self) -> Result<(), io::Error> {
        if let Some(ref peripheral_manager) = self.peripheral_manager {
            unsafe {
                let _: () = msg_send![**peripheral_manager, stopAdvertising];
                debug!("Stopped CoreBluetooth advertisement");
            }
        }
        self.peripheral_manager = None;
        Ok(())
    }

    pub async fn start_advertising(mut self, data: AdvertisementData) -> Result<AdvertisingGuard, String> {
        //self.stop_advertising();

        // Initialize CBPeripheralManager if not already created
        if self.peripheral_manager.is_none() {
            println!("creating new peripheral_manager");
            let peripheral_manager: *mut Object = unsafe {
                let manager: *mut Object = msg_send![class!(CBPeripheralManager), alloc];
                msg_send![manager, init]
            };
            self.peripheral_manager = Some(unsafe { StrongPtr::new(peripheral_manager) });
        }

        if let Some(ref peripheral_manager) = self.peripheral_manager {
            // debug!("Starting CoreBluetooth advertisement");
            // let is_advertising: bool = unsafe { msg_send![**peripheral_manager, isAdvertising] };
            // debug!("Peripheral Manager is advertising: {}", is_advertising);

            // Create an NSMutableDictionary and add manufacturer data
            let advertisement_data = create_mutable_dictionary();
            if let Some(manufacturer_data) = data.manufacturer_data {
                // Combine the company ID with the manufacturer data
                let mut combined_data = Vec::with_capacity(2 + manufacturer_data.data.len());
                let c = manufacturer_data.company_id.to_le_bytes();
                combined_data.extend_from_slice(&[c[1], c[0]]);
                //combined_data.extend_from_slice(&[0x69u8,0x69u8]);
                combined_data.extend_from_slice(&manufacturer_data.data);
                debug!("Final Manufacturer Data: {:x?}", combined_data);
                add_data_to_dict(advertisement_data, "kCBAdvDataManufacturerData", &combined_data);
                debug!("Setting kCBAdvDataManufacturerData: {:x?}", combined_data);
            }
            debug!("starting ADVERT");
            unsafe {
                let description: *mut Object = msg_send![advertisement_data, description];
                let c_str: *const i8 = msg_send![description, UTF8String];
                if !c_str.is_null() {
                    let ad_description = std::ffi::CStr::from_ptr(c_str).to_string_lossy().into_owned();
                    debug!("Advertisement Dictionary Description: {}", ad_description);
                } else {
                    debug!("Failed to get Advertisement Dictionary Description");
                }
            }

            // Start advertising
            unsafe {
                let _: () = msg_send![**peripheral_manager, startAdvertising: advertisement_data];
            }
            debug!("done ADVERT");

            return Ok(AdvertisingGuard { advertisement: self });
        }
        Err("Failed to start CoreBluetooth advertising".to_owned())
    }
}

impl Drop for AdvertisementImpl {
    fn drop(&mut self) {
        println!("AdvertisementImpl dropped.");
    }
}

fn create_mutable_dictionary() -> *mut Object {
    let dict_class = Class::get("NSMutableDictionary").expect("NSMutableDictionary class not found");
    unsafe { msg_send![dict_class, dictionary] }
}

fn add_data_to_dict(dict: *mut Object, key: &str, value: &[u8]) {
    debug!("Adding to Dictionary - Key: {}, Value: {:x?}", key, value);
    let ns_key = NSString::from_str(key);
    debug!("Adding to Dictionary - Key: {}, Value: {:x?}", key, value);
    let ns_value = NSData::from_vec(value);
    debug!("Adding to Dictionary - Key: {}, Value: {:x?}", key, value);
    unsafe {
        let _: () = msg_send![dict, setObject: ns_value forKey: ns_key];
    }
}

// Helper function to convert Rust string to NSString
pub struct NSString;
impl NSString {
    pub fn from_str(s: &str) -> *mut Object {
        let ns_string_class = Class::get("NSString").expect("NSString class not found");
        let ns_string: *mut Object = unsafe { msg_send![ns_string_class, alloc] };
        unsafe { msg_send![ns_string, initWithUTF8String: s.as_ptr()] }
    }
}

// Helper function to convert Vec<u8> to NSData
pub struct NSData;
impl NSData {
    pub fn from_vec(data: &[u8]) -> *mut Object {
        let data_class = Class::get("NSData").expect("NSData class not found");
        unsafe { msg_send![data_class, dataWithBytes: data.as_ptr() length: data.len()] }
    }
}

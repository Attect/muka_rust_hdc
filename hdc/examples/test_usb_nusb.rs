use nusb::MaybeFuture;
use nusb::transfer::{Bulk, Out};
use std::io::Write;

fn main() {
    let mut target_dev = None;
    let devices: Vec<_> = nusb::list_devices().wait().unwrap().collect();
    for dev in devices {
        if dev.vendor_id() != 0x12D1 || dev.product_id() != 0x1101 {
            continue;
        }
        let found = dev.interfaces().any(|interface| {
            interface.class() == 0xFF && interface.subclass() == 0x50 && interface.protocol() == 0x01
        });
        if found {
            println!("Found matching device");
            target_dev = Some(dev);
            break;
        }
    }
    let dev = target_dev.expect("No matching device");
    let info = dev.open().wait().unwrap();
    let interface = info.claim_interface(0).wait().unwrap();

    let mut writer = interface.endpoint::<Bulk, Out>(0x01).unwrap().writer(4096);
    let data = [0x55, 0x42, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    println!("Writing {} bytes: {:?}", data.len(), data);
    match writer.write_all(&data) {
        Ok(_) => {
            match writer.flush() {
                Ok(_) => println!("Write flushed successfully"),
                Err(e) => println!("Flush failed: {:?}", e),
            }
        }
        Err(e) => println!("Write failed: {:?}", e),
    }
}

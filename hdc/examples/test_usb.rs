use std::time::Duration;

fn main() {
    let devices = rusb::devices().unwrap();
    for device in devices.iter() {
        let desc = device.device_descriptor().unwrap();
        if desc.vendor_id() != 0x12D1 || desc.product_id() != 0x1101 {
            continue;
        }
        println!("Found device {:04x}:{:04x}", desc.vendor_id(), desc.product_id());
        let handle = device.open().unwrap();
        let config = device.config_descriptor(0).unwrap();
        for interface in config.interfaces() {
            for alt in interface.descriptors() {
                if alt.class_code() == 0xFF && alt.sub_class_code() == 0x50 && alt.protocol_code() == 0x01 {
                    let iface = interface.number();
                    let mut bulk_out = None;
                    for ep in alt.endpoint_descriptors() {
                        if ep.transfer_type() == rusb::TransferType::Bulk && ep.direction() == rusb::Direction::Out {
                            bulk_out = Some(ep.address());
                        }
                    }
                    if let Some(out_addr) = bulk_out {
                        println!("Claiming interface {}, Bulk OUT=0x{:02x}", iface, out_addr);
                        if let Err(e) = handle.claim_interface(iface) {
                            println!("Claim failed: {}", e);
                            continue;
                        }
                        let _ = handle.clear_halt(out_addr);

                        // Try 11 bytes first
                        let data11 = [0x55, 0x42, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
                        println!("Writing 11 bytes: {:?}", data11);
                        match handle.write_bulk(out_addr, &data11, Duration::from_secs(5)) {
                            Ok(n) => println!("Wrote {} bytes", n),
                            Err(e) => println!("Write 11 bytes failed: {:?}", e),
                        }

                        // Try 512 bytes
                        let data512 = vec![0x55u8; 512];
                        println!("Writing 512 bytes");
                        match handle.write_bulk(out_addr, &data512, Duration::from_secs(5)) {
                            Ok(n) => println!("Wrote {} bytes", n),
                            Err(e) => println!("Write 512 bytes failed: {:?}", e),
                        }

                        handle.release_interface(iface).unwrap();
                        return;
                    }
                }
            }
        }
    }
    println!("No matching device found");
}

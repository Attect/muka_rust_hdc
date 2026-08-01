fn main() {
    unsafe {
        let mut ctx: *mut libusb1_sys::libusb_context = std::ptr::null_mut();
        let r = libusb1_sys::libusb_init(&mut ctx);
        if r < 0 {
            panic!("libusb_init failed: {}", r);
        }

        let mut devs: *const *mut libusb1_sys::libusb_device = std::ptr::null();
        let cnt = libusb1_sys::libusb_get_device_list(ctx, &mut devs);
        if cnt < 0 {
            panic!("libusb_get_device_list failed: {}", cnt);
        }

        let mut target_dev: *mut libusb1_sys::libusb_device = std::ptr::null_mut();
        for i in 0..cnt {
            let dev = *devs.offset(i);
            let mut desc: libusb1_sys::libusb_device_descriptor = std::mem::zeroed();
            if libusb1_sys::libusb_get_device_descriptor(dev, &mut desc) < 0 {
                continue;
            }
            if desc.idVendor != 0x12D1 || desc.idProduct != 0x1101 {
                continue;
            }
            println!("Found device {:04x}:{:04x}", desc.idVendor, desc.idProduct);
            target_dev = dev;
            break;
        }

        if target_dev.is_null() {
            panic!("Device not found");
        }

        // First open
        let mut handle1: *mut libusb1_sys::libusb_device_handle = std::ptr::null_mut();
        if libusb1_sys::libusb_open(target_dev, &mut handle1) < 0 {
            panic!("First open failed");
        }

        let mut config: *const libusb1_sys::libusb_config_descriptor = std::ptr::null();
        if libusb1_sys::libusb_get_active_config_descriptor(target_dev, &mut config) < 0 {
            panic!("config descriptor failed");
        }

        let mut iface_num = 0i32;
        let mut bulk_out = 0u8;
        for j in 0..(*config).bNumInterfaces as isize {
            let iface = &*(*config).interface.offset(j);
            if iface.num_altsetting < 1 {
                continue;
            }
            let alt = &*iface.altsetting;
            if alt.bInterfaceClass != 0xFF || alt.bInterfaceSubClass != 0x50 || alt.bInterfaceProtocol != 0x01 {
                continue;
            }
            iface_num = alt.bInterfaceNumber as i32;
            for k in 0..alt.bNumEndpoints as isize {
                let ep = &*alt.endpoint.offset(k);
                if (ep.bmAttributes & 0x03) == 2 && ep.bEndpointAddress & 0x80 == 0 {
                    bulk_out = ep.bEndpointAddress;
                }
            }
            break;
        }

        println!("First open: claiming interface {}", iface_num);
        libusb1_sys::libusb_claim_interface(handle1, iface_num);

        // Wait a bit before second open (like official timer delay)
        std::thread::sleep(std::time::Duration::from_secs(3));

        // Second open
        let mut handle: *mut libusb1_sys::libusb_device_handle = std::ptr::null_mut();
        let r = libusb1_sys::libusb_open(target_dev, &mut handle);
        println!("Second open returned: {}", r);
        if r < 0 {
            println!("Second open failed, using first handle instead");
            handle = handle1;
        } else {
            println!("Second open: claiming interface {}", iface_num);
            let r = libusb1_sys::libusb_claim_interface(handle, iface_num);
            println!("Second claim returned: {}", r);
        }

        let data = [0x55u8, 0x42, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        println!("Writing {} bytes via libusb_bulk_transfer...", data.len());
        let mut transferred = 0i32;
        let r = libusb1_sys::libusb_bulk_transfer(
            handle,
            bulk_out,
            data.as_ptr() as *mut u8,
            data.len() as i32,
            &mut transferred,
            5000,
        );
        if r < 0 {
            println!("libusb_bulk_transfer failed: {} (transferred={})", r, transferred);
        } else {
            println!("Success! transferred={}", transferred);
        }

        if handle != handle1 {
            libusb1_sys::libusb_release_interface(handle, iface_num);
            libusb1_sys::libusb_close(handle);
        }
        libusb1_sys::libusb_release_interface(handle1, iface_num);
        libusb1_sys::libusb_close(handle1);

        libusb1_sys::libusb_free_config_descriptor(config);
        libusb1_sys::libusb_free_device_list(devs, 1);
        libusb1_sys::libusb_exit(ctx);
    }
}

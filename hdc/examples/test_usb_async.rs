use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

fn main() {
    unsafe {
        let mut ctx: *mut libusb1_sys::libusb_context = std::ptr::null_mut();
        let r = libusb1_sys::libusb_init(&mut ctx);
        if r < 0 {
            panic!("libusb_init failed: {}", r);
        }

        let ctx_usize = ctx as usize;
        let _event_thread = thread::spawn(move || {
            let ctx = ctx_usize as *mut libusb1_sys::libusb_context;
            loop {
                let mut tv: libc::timeval = std::mem::zeroed();
                tv.tv_sec = 30;
                tv.tv_usec = 0;
                libusb1_sys::libusb_handle_events_timeout(ctx, &mut tv);
            }
        });

        let mut devs: *const *mut libusb1_sys::libusb_device = std::ptr::null();
        let cnt = libusb1_sys::libusb_get_device_list(ctx, &mut devs);
        if cnt < 0 {
            panic!("libusb_get_device_list failed: {}", cnt);
        }

        let mut handle: *mut libusb1_sys::libusb_device_handle = std::ptr::null_mut();
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
            if libusb1_sys::libusb_open(dev, &mut handle) < 0 {
                continue;
            }

            let mut config: *const libusb1_sys::libusb_config_descriptor = std::ptr::null();
            if libusb1_sys::libusb_get_active_config_descriptor(dev, &mut config) < 0 {
                libusb1_sys::libusb_close(handle);
                handle = std::ptr::null_mut();
                continue;
            }

            let mut found = false;
            for j in 0..(*config).bNumInterfaces as isize {
                let iface = &*(*config).interface.offset(j);
                if iface.num_altsetting < 1 {
                    continue;
                }
                let alt = &*iface.altsetting;
                if alt.bInterfaceClass != 0xFF || alt.bInterfaceSubClass != 0x50 || alt.bInterfaceProtocol != 0x01 {
                    continue;
                }
                let iface_num = alt.bInterfaceNumber;
                let mut bulk_out = 0u8;
                for k in 0..alt.bNumEndpoints as isize {
                    let ep = &*alt.endpoint.offset(k);
                    if (ep.bmAttributes & 0x03) == 2 { // Bulk
                        if ep.bEndpointAddress & 0x80 != 0 {
                            // IN
                        } else {
                            bulk_out = ep.bEndpointAddress;
                        }
                    }
                }
                if bulk_out != 0 {
                    println!("Claiming interface {}, Bulk OUT=0x{:02x}", iface_num, bulk_out);
                    let r = libusb1_sys::libusb_claim_interface(handle, iface_num as i32);
                    if r < 0 {
                        println!("claim failed: {}", r);
                        continue;
                    }
                    found = true;

                    let completed = Arc::new(AtomicBool::new(false));
                    let completed2 = completed.clone();

                    let data = [0x55u8, 0x42, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
                    let transfer = libusb1_sys::libusb_alloc_transfer(0);
                    libusb1_sys::libusb_fill_bulk_transfer(
                        transfer,
                        handle,
                        bulk_out,
                        data.as_ptr() as *mut u8,
                        data.len() as i32,
                        callback,
                        Arc::into_raw(completed2) as *mut _,
                        30000,
                    );

                    println!("Submitting transfer at {:?}...", std::time::Instant::now());
                    let r = libusb1_sys::libusb_submit_transfer(transfer);
                    if r < 0 {
                        println!("submit failed: {}", r);
                    } else {
                        let start = std::time::Instant::now();
                        while !completed.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(35) {
                            thread::sleep(Duration::from_millis(10));
                        }
                        println!("Wait finished after {:?}", start.elapsed());
                        if !completed.load(Ordering::Relaxed) {
                            println!("Timeout waiting for transfer");
                        }
                    }

                    if !completed.load(Ordering::Relaxed) {
                        libusb1_sys::libusb_cancel_transfer(transfer);
                        thread::sleep(Duration::from_millis(500));
                    }
                    libusb1_sys::libusb_free_transfer(transfer);

                    libusb1_sys::libusb_release_interface(handle, iface_num as i32);
                    break;
                }
            }
            libusb1_sys::libusb_free_config_descriptor(config);
            if found {
                break;
            }
            if !handle.is_null() {
                libusb1_sys::libusb_close(handle);
                handle = std::ptr::null_mut();
            }
        }

        libusb1_sys::libusb_free_device_list(devs, 1);
        if !handle.is_null() {
            libusb1_sys::libusb_close(handle);
        }
        libusb1_sys::libusb_exit(ctx);
    }
}

extern "system" fn callback(transfer: *mut libusb1_sys::libusb_transfer) {
    unsafe {
        let t = &*transfer;
        println!("Transfer completed at {:?}! status={}, actual_length={}", std::time::Instant::now(), t.status, t.actual_length);
        if !t.user_data.is_null() {
            let completed = Arc::from_raw(t.user_data as *mut AtomicBool);
            completed.store(true, Ordering::Relaxed);
            let _ = Arc::into_raw(completed);
        }
    }
}

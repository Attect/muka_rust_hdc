// Direct async bulk transfer test matching official tool's pattern exactly.
// Uses libusb1_sys directly with a dedicated event handling thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

        let mut handle: *mut libusb1_sys::libusb_device_handle = std::ptr::null_mut();
        if libusb1_sys::libusb_open(target_dev, &mut handle) < 0 {
            panic!("libusb_open failed");
        }

        let mut config: *const libusb1_sys::libusb_config_descriptor = std::ptr::null();
        if libusb1_sys::libusb_get_active_config_descriptor(target_dev, &mut config) < 0 {
            panic!("config descriptor failed");
        }

        let mut iface_num = 0i32;
        let mut bulk_out = 0u8;
        let mut bulk_in = 0u8;
        let mut w_max_packet = 0u16;
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
            if alt.bNumEndpoints > 0 {
                w_max_packet = (*alt.endpoint).wMaxPacketSize;
            }
            for k in 0..alt.bNumEndpoints as isize {
                let ep = &*alt.endpoint.offset(k);
                if (ep.bmAttributes & 0x03) == 2 {
                    if ep.bEndpointAddress & 0x80 == 0 {
                        bulk_out = ep.bEndpointAddress;
                    } else {
                        bulk_in = ep.bEndpointAddress;
                    }
                }
            }
            break;
        }

        println!("Interface={}, BulkOUT=0x{:02x}, BulkIN=0x{:02x}, wMaxPacket={}",
                 iface_num, bulk_out, bulk_in, w_max_packet);

        println!("Claiming interface...");
        let r = libusb1_sys::libusb_claim_interface(handle, iface_num);
        println!("claim_interface returned: {}", r);

        // NOTE: Official tool does NOT call set_alternate_setting or clear_halt!
        // Only claim_interface is called after open.

        // Start event handling thread (like official tool)
        let ctx_usize = ctx as usize;
        let stop_events = Arc::new(AtomicBool::new(false));
        let stop_events_clone = stop_events.clone();
        let event_thread = thread::spawn(move || {
            let ctx = ctx_usize as *mut libusb1_sys::libusb_context;
            while !stop_events_clone.load(Ordering::Relaxed) {
                let mut tv = libc::timeval {
                    tv_sec: 0,
                    tv_usec: 100_000,
                };
                libusb1_sys::libusb_handle_events_timeout_completed(ctx, &mut tv, std::ptr::null_mut());
            }
        });

        // Give event thread time to start
        thread::sleep(Duration::from_millis(100));

        // Test 1: Control transfer (should work)
        println!("\n=== Test 1: Control transfer ===");
        let mut buf = [0u8; 18];
        let r = libusb1_sys::libusb_control_transfer(
            handle,
            0x80, // GET_DESCRIPTOR, DEVICE
            0x06,
            0x0100,
            0,
            buf.as_mut_ptr(),
            18,
            5000,
        );
        println!("control_transfer returned: {} (expected 18)", r);

        // Test 2: Async bulk OUT write (11 bytes soft-reset header)
        println!("\n=== Test 2: Async bulk OUT ===");
        let data = vec![0x55u8, 0x42, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let transfer = libusb1_sys::libusb_alloc_transfer(0);
        if transfer.is_null() {
            panic!("alloc_transfer failed");
        }

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        extern "system" fn callback(transfer: *mut libusb1_sys::libusb_transfer) {
            unsafe {
                let ctx = (*transfer).user_data as *mut AtomicBool;
                println!("Callback: status={}, actual_length={}", (*transfer).status, (*transfer).actual_length);
                (*ctx).store(true, Ordering::Relaxed);
            }
        }

        libusb1_sys::libusb_fill_bulk_transfer(
            transfer,
            handle,
            bulk_out,
            data.as_ptr() as *mut u8,
            data.len() as i32,
            callback,
            Arc::into_raw(completed_clone) as *mut _,
            30000, // 30s timeout matching official tool
        );

        println!("Submitting transfer...");
        let r = libusb1_sys::libusb_submit_transfer(transfer);
        println!("submit_transfer returned: {}", r);

        if r == 0 {
            println!("Waiting for completion...");
            let start = std::time::Instant::now();
            while !completed.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
                if start.elapsed() > Duration::from_secs(35) {
                    println!("TIMEOUT waiting for callback!");
                    break;
                }
            }
            println!("Done waiting. completed={}", completed.load(Ordering::Relaxed));
        }

        // Cleanup
        stop_events.store(true, Ordering::Relaxed);
        event_thread.join().unwrap();

        libusb1_sys::libusb_release_interface(handle, iface_num);
        libusb1_sys::libusb_close(handle);
        libusb1_sys::libusb_free_config_descriptor(config);
        libusb1_sys::libusb_free_device_list(devs, 1);
        libusb1_sys::libusb_exit(ctx);

        println!("\nTest completed.");
    }
}

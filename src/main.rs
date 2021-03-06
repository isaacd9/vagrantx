extern crate virtualization_rs;

use block::{Block, ConcreteBlock};
use libc::{sleep, tcgetattr, tcsetattr, ECHO, ICANON, ICRNL, TCSANOW};
use objc::rc::StrongPtr;
use objc::{msg_send, sel, sel_impl};
use serde::{Deserialize, Serialize};
use std::error;
use std::fs::{canonicalize, File};
use std::io::Read;
use std::mem::MaybeUninit;
use std::sync::{Arc, RwLock};
use virtualization_rs::virtualization::boot_loader;
use virtualization_rs::{
    base::{dispatch_async, dispatch_queue_create, Id, NSError, NSFileHandle, NIL},
    virtualization::{
        boot_loader::VZLinuxBootLoaderBuilder,
        entropy_device::VZVirtioEntropyDeviceConfiguration,
        memory_device::VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
        network_device::{
            VZMACAddress, VZNATNetworkDeviceAttachment, VZVirtioNetworkDeviceConfiguration,
        },
        serial_port::{
            VZFileHandleSerialPortAttachmentBuilder, VZVirtioConsoleDeviceSerialPortConfiguration,
        },
        storage_device::{
            VZDiskImageStorageDeviceAttachmentBuilder, VZVirtioBlockDeviceConfiguration,
        },
        virtual_machine::{VZVirtualMachine, VZVirtualMachineConfigurationBuilder},
    },
};

use std::path::{Path, PathBuf};
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(name = "simplevm")]
struct Opt {
    #[structopt(parse(from_os_str))]
    config: PathBuf,
}

fn default_cpu() -> usize {
    2
}

fn default_mem_size() -> usize {
    2147483648
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boot {
    kernel: PathBuf,
    initrd: PathBuf,

    #[serde(default)]
    command_line: String,

    #[serde(default)]
    disks: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    boot: Boot,

    #[serde(default = "default_cpu")]
    cpu_count: usize,

    #[serde(default)]
    additional_disks: Vec<PathBuf>,

    #[serde(default = "default_mem_size")]
    memory_size: usize,
}

fn build_console_configuration() -> VZVirtioConsoleDeviceSerialPortConfiguration {
    let file_handle_for_reading = NSFileHandle::file_handle_with_standard_input();

    unsafe {
        let mut attributes = MaybeUninit::uninit();
        let r = tcgetattr(
            msg_send![*file_handle_for_reading.0, fileDescriptor],
            attributes.as_mut_ptr(),
        );
        let mut init_attributes = attributes.assume_init_mut();

        init_attributes.c_iflag &= !ICRNL;
        init_attributes.c_lflag &= !(ICANON | ECHO);

        let r = tcsetattr(
            msg_send![*file_handle_for_reading.0, fileDescriptor],
            TCSANOW,
            attributes.as_ptr(),
        );
    };

    let file_handle_for_writing = NSFileHandle::file_handle_with_standard_output();
    let attachement = VZFileHandleSerialPortAttachmentBuilder::new()
        .file_handle_for_reading(file_handle_for_reading)
        .file_handle_for_writing(file_handle_for_writing)
        .build();

    VZVirtioConsoleDeviceSerialPortConfiguration::new(attachement)
}

fn build_boot_loader(
    kernel: &Path,
    initrd: &Path,
    cmd_line: &str,
) -> boot_loader::VZLinuxBootLoader {
    VZLinuxBootLoaderBuilder::new()
        .kernel_url(
            canonicalize(&kernel)
                .unwrap()
                .into_os_string()
                .into_string()
                .unwrap(),
        )
        .initial_ramdisk_url(
            canonicalize(&initrd)
                .unwrap()
                .into_os_string()
                .into_string()
                .unwrap(),
        )
        .command_line(cmd_line)
        .build()
}

fn build_block_devices(
    disks: &[PathBuf],
) -> Result<Vec<VZVirtioBlockDeviceConfiguration>, NSError> {
    let mut block_devices = Vec::with_capacity(disks.len());
    for disk in disks {
        let block_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
            .path(
                canonicalize(disk)
                    .unwrap()
                    .into_os_string()
                    .into_string()
                    .unwrap(),
            )
            .read_only(false)
            .build()?;
        let block_device = VZVirtioBlockDeviceConfiguration::new(block_attachment);
        block_devices.push(block_device);
    }
    Ok(block_devices)
}

fn load_config(config_file: &PathBuf) -> Result<Config, Box<dyn error::Error>> {
    let mut file = File::open(config_file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    serde_json::from_str(&contents).map_err(|e| e.into())
}

fn main() {
    let opt = Opt::from_args();

    let config = load_config(&opt.config).expect("could not read config");

    if !VZVirtualMachine::supported() {
        println!("not supported");
        return;
    }

    let entropy = VZVirtioEntropyDeviceConfiguration::new();
    let memory_balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();

    let network_attachment = VZNATNetworkDeviceAttachment::new();
    let mut network_device = VZVirtioNetworkDeviceConfiguration::new(network_attachment);
    network_device.set_mac_address(VZMACAddress::random_locally_administered_address());

    let boot_loader = build_boot_loader(
        &config.boot.kernel,
        &config.boot.initrd,
        &config.boot.command_line,
    );

    let block_devices = match build_block_devices(
        &config
            .boot
            .disks
            .iter()
            .cloned()
            .chain(config.additional_disks.iter().cloned())
            .collect::<Vec<_>>(),
    ) {
        Ok(devices) => devices,
        Err(err) => {
            err.dump();
            return;
        }
    };

    let conf = VZVirtualMachineConfigurationBuilder::new()
        .boot_loader(boot_loader)
        .cpu_count(config.cpu_count)
        .memory_size(config.memory_size)
        .entropy_devices(vec![entropy])
        .memory_balloon_devices(vec![memory_balloon])
        .network_devices(vec![network_device])
        .serial_ports(vec![build_console_configuration()])
        .storage_devices(block_devices)
        .build();

    match conf.validate_with_error() {
        Ok(_) => {
            let label = std::ffi::CString::new("second").unwrap();
            let queue = unsafe { dispatch_queue_create(label.as_ptr(), NIL) };
            let vm = Arc::new(RwLock::new(VZVirtualMachine::new(conf, queue)));
            let dispatch_block = ConcreteBlock::new(move || {
                let completion_handler = ConcreteBlock::new(|err: Id| {
                    if err != NIL {
                        let error = unsafe { NSError(StrongPtr::new(err)) };
                        error.dump();
                    }
                });
                let completion_handler = completion_handler.copy();
                let completion_handler: &Block<(Id,), ()> = &completion_handler;
                vm.write()
                    .unwrap()
                    .start_with_completion_handler(completion_handler);
            });
            let dispatch_block = dispatch_block.copy();
            let dispatch_block: &Block<(), ()> = &dispatch_block;
            unsafe {
                dispatch_async(queue, dispatch_block);
            }
            loop {
                unsafe {
                    sleep(1000);
                }
            }
        }
        Err(e) => {
            e.dump();
            return;
        }
    }
}

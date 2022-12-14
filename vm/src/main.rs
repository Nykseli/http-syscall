mod request;
mod system_call_names;

use byteorder::{LittleEndian, WriteBytesExt};
use linux_personality::personality;
use nix::sys::ptrace::{self, AddressType};
use nix::sys::wait::wait;
use nix::unistd::{fork, ForkResult, Pid};
use std::env;

use std::os::unix::process::CommandExt;
use std::process::{exit, Command};

use libc::{c_long, c_void, user_regs_struct};

fn read_string(pid: Pid, address: AddressType) -> String {
    let mut string = String::new();
    // Move 8 bytes up each time for next read.
    let mut count = 0;
    let word_size = 8;

    'done: loop {
        let mut bytes: Vec<u8> = vec![];
        let address = unsafe { address.offset(count) };

        let res: c_long = ptrace::read(pid, address).unwrap_or_else(|err| {
            panic!("Failed to read data for pid {}: {}", pid, err);
        });
        bytes.write_i64::<LittleEndian>(res).unwrap_or_else(|err| {
            panic!("Failed to write {} as i64 LittleEndian: {}", res, err);
        });

        for b in bytes {
            if b != 0 {
                string.push(b as char);
            } else {
                break 'done;
            }
        }
        count += word_size;
    }

    string
}

fn read_data(pid: Pid, address: AddressType, length: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    // Move 8 bytes up each time for next read.
    let mut count = 0;
    let _word_size = 8;

    'done: loop {
        let mut bytes: Vec<u8> = vec![];
        let address = unsafe { address.offset(count) };

        let res: c_long = ptrace::read(pid, address).unwrap_or_else(|err| {
            panic!("Failed to read data for pid {}: {}", pid, err);
        });
        bytes.write_i64::<LittleEndian>(res).unwrap_or_else(|err| {
            panic!("Failed to write {} as i64 LittleEndian: {}", res, err);
        });

        for b in bytes {
            if count >= length as isize {
                break 'done;
            }

            buf.push(b);
            count += 1;
        }
    }

    buf
}

fn write_data(pid: Pid, address: AddressType, data: &Vec<u8>) {
    // Move 8 bytes up each time for next read.
    let mut count: usize = 0;
    let word_size: usize = 8;

    'done: loop {
        if count >= data.len() {
            break 'done;
        }

        let mut bytes: [u8; 8] = [0; 8];
        let address = unsafe { address.offset(count as isize) };

        let next_bytes = if count + word_size <= data.len() {
            word_size
        } else {
            data.len() - count
        };

        for ii in 0..next_bytes {
            bytes[ii] = data[count + ii];
        }

        // NOTE: this only works on 64bit little endian
        let data_word = u64::from_le_bytes(bytes);
        unsafe {
            ptrace::write(pid, address, data_word as *mut c_void).unwrap();
        }

        count += word_size;
    }
}

fn handle_syscall_end<T: http_data::SysCallResp>(child: Pid, resp: &T) {
    if let Ok(mut new_regs) = ptrace::getregs(child) {
        new_regs.rax = resp.ret_value() as u64;
        ptrace::setregs(child, new_regs).unwrap();
    }
}

fn handle_syscall(child: Pid, regs: user_regs_struct) {
    println!(
        "Handle: {:?}",
        system_call_names::SYSTEM_CALL_NAMES[(regs.orig_rax) as usize]
    );

    // TODO: use fs_fh_file_handle in fuse fs to test that this actually prevents the syscall
    if http_data::is_implemented(regs.orig_rax) {
        let mut tmp = regs;
        tmp.rax = u64::MAX;
        // Setting orig_rax is the one that prevents (a valid) syscall from happening
        // but might as well set the rax to invalid value too
        tmp.orig_rax = u64::MAX;
        ptrace::setregs(child, tmp).unwrap();
    }

    ptrace::step(child, None).unwrap();
    wait().unwrap();

    if regs.orig_rax == http_data::SysCallNum::Read {
        println!("Sending read syscall request...");
        let write_addr = regs.rsi;
        let resp: http_data::ReadResp = request::unchecked_request(
            "read",
            &http_data::ReadRequest {
                fd: regs.rdi as i64,
                nbytes: regs.rdx,
            },
        );
        println!("Got a read response:\n{:#?}\n", resp);

        let byte_buf = http_data::decode_buffer(&resp.data);
        write_data(child, write_addr as *mut c_void, &byte_buf);

        handle_syscall_end(child, &resp);
    } else if regs.orig_rax == http_data::SysCallNum::Open {
        println!("Sending read open request...");
        let path = read_string(child, regs.rdi as *mut c_void);
        let resp: http_data::OpenResp = request::unchecked_request(
            "open",
            &http_data::OpenRequest {
                path: path,
                oflag: regs.rsi,
                mode: regs.rdx,
            },
        );
        println!("Got a open response:\n{:#?}\n", resp);

        handle_syscall_end(child, &resp);
    } else if regs.orig_rax == http_data::SysCallNum::Close {
        println!("Sending read close request...");
        let resp: http_data::CloseResp = request::unchecked_request(
            "close",
            &http_data::CloseRequest {
                fd: regs.rdi as i64,
            },
        );
        println!("Got a close response:\n{:#?}\n", resp);

        handle_syscall_end(child, &resp);
    } else if regs.orig_rax == http_data::SysCallNum::Write {
        println!("Sending read write request...");
        let write_data = read_data(child, regs.rsi as *mut c_void, regs.rdx);
        let buf = http_data::encode_buffer(&write_data, regs.rdx as i64);

        let resp: http_data::WriteResp = request::unchecked_request(
            "write",
            &http_data::WriteRequest {
                fd: regs.rdi as i64,
                buf: buf,
                nbytes: regs.rdx,
            },
        );
        println!("Got a write response:\n{:#?}\n", resp);

        handle_syscall_end(child, &resp);
    }
}

fn run_tracer(child: Pid) -> Result<(), nix::errno::Errno> {
    // Handle the initial execve
    wait().unwrap();

    loop {
        // Syscall will error out when the program finnishes
        // TODO: better error handling
        if let Err(_) = ptrace::syscall(child, None) {
            return Ok(());
        }

        wait()?;

        let regs = ptrace::getregs(child)?;
        handle_syscall(child, regs);
    }
}

fn run_tracee(command: &str) {
    ptrace::traceme().unwrap();
    personality(linux_personality::ADDR_NO_RANDOMIZE).unwrap();

    Command::new(command).exec();

    exit(0)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} <executable>", args[0]);
        return;
    }

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            run_tracee(&args[1]);
        }

        Ok(ForkResult::Parent { child }) => {
            if let Err(e) = run_tracer(child) {
                println!("Tracer failed: '{:?}'", e);
            }
        }

        Err(err) => {
            panic!("[main] fork() failed: {}", err);
        }
    }
}

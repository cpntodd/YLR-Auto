#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// One EBML element, with its size written as an 8-byte integer.
pub fn ebml(element_id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut element = element_id.to_vec();
    element.push(0x01);
    element.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    element.extend_from_slice(payload);
    element
}

/// A minimal MKV header carrying `seconds` of duration, padded by `pad` bytes.
pub fn fake_mkv_padded(seconds: f64, pad: usize) -> Vec<u8> {
    let mut info = ebml(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]); // 1 tick = 1 ms
    info.extend(ebml(&[0x44, 0x89], &(seconds * 1000.0).to_be_bytes()));
    let mut segment = ebml(&[0x15, 0x49, 0xa9, 0x66], &info);
    segment.extend(ebml(&[0x1f, 0x43, 0xb6, 0x75], &vec![0; pad]));
    let mut file = ebml(&[0x1a, 0x45, 0xdf, 0xa3], b"");
    file.extend(ebml(&[0x18, 0x53, 0x80, 0x67], &segment));
    file
}

pub fn fake_mkv(seconds: f64) -> Vec<u8> {
    fake_mkv_padded(seconds, 0)
}

pub fn mp4_box(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut boxed = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    boxed.extend_from_slice(name);
    boxed.extend_from_slice(payload);
    boxed
}

pub fn fake_mp4(seconds: f64) -> Vec<u8> {
    let mut mvhd = vec![0; 12];
    mvhd.extend_from_slice(&1000_u32.to_be_bytes());
    mvhd.extend_from_slice(&((seconds * 1000.0) as u32).to_be_bytes());
    mvhd.extend_from_slice(&[0; 80]);
    let mut file = mp4_box(b"ftyp", b"isom");
    file.extend(mp4_box(b"moov", &mp4_box(b"mvhd", &mvhd)));
    file
}

pub fn set_age(path: &Path, age_seconds: u64) {
    let time = SystemTime::now() - Duration::from_secs(age_seconds);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open test file");
    file.set_modified(time).expect("set test file time");
}

use std::fs::remove_file;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;

use crate::bs::decompress_lz4_into;
use crate::common::{DConfig, DImage, DImageD, PixelType};
use crate::exceptions::{ConnectionError, DecompressError};
use bincode::serialize;
use ipc_test::{SHMHandle, SHMInfo, SharedSlabAllocator, Slot, SlotForWriting, SlotInfo};
use log::{debug, trace};
use nix::poll::{PollFd, PollFlags};
use numpy::PyArray3;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::{
    exceptions,
    prelude::*,
    types::{PyBytes, PyType},
};
use sendfd::{RecvWithFd, SendWithFd};
use serde::{Deserialize, Serialize};

#[derive(PartialEq, Eq, Clone, Serialize, Deserialize, Debug)]
pub struct FrameMeta {
    pub dimage: DImage,
    pub dimaged: DImageD,
    pub dconfig: DConfig,
    pub data_length_bytes: usize,
}

impl FrameMeta {
    /// Get the number of elements in this frame (`prod(shape)`)
    pub fn get_size(&self) -> u64 {
        self.dimaged.shape.iter().product()
    }
}

pub struct FrameStackForWriting {
    slot: SlotForWriting,
    meta: Vec<FrameMeta>,
    capacity: usize,
    bytes_per_frame: usize,
}

impl FrameStackForWriting {
    pub fn new(slot: SlotForWriting, capacity: usize, bytes_per_frame: usize) -> Self {
        FrameStackForWriting {
            slot,
            capacity, // number of frames
            bytes_per_frame,
            meta: Vec::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.meta.len()
    }

    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// take frame metadata and put it into our list
    /// and copy data into shared the right shm position
    /// (we can't have zero-copy directly into shm with 0mq sadly, as far as I
    /// can tell)
    /// TODO: in a general library, this should also have a zero-copy api available!
    pub fn frame_done(
        &mut self,
        dimage: DImage,
        dimaged: DImageD,
        dconfig: DConfig,
        data: &[u8],
    ) -> FrameMeta {
        let idx = self.len();
        // FIXME: alignment per frame?
        let start = idx * self.bytes_per_frame;
        let stop = start + self.bytes_per_frame;
        let dest = &mut self.slot.as_slice_mut()[start..stop];
        // FIXME: return error on slice length mismatch, don't panic
        dest[..data.len()].copy_from_slice(data);
        let meta = FrameMeta {
            dimage,
            dimaged,
            dconfig,
            data_length_bytes: data.len(),
        };
        self.meta.push(meta.clone());
        meta
    }

    pub fn writing_done(self, shm: &mut SharedSlabAllocator) -> FrameStackHandle {
        let slot_info = shm.writing_done(self.slot);

        FrameStackHandle {
            slot: slot_info,
            meta: self.meta,
            bytes_per_frame: self.bytes_per_frame,
        }
    }
}

/// serializable handle for a stack of frames that live in shm
#[pyclass]
#[derive(PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FrameStackHandle {
    slot: SlotInfo,
    meta: Vec<FrameMeta>,
    bytes_per_frame: usize,
}

impl FrameStackHandle {
    pub fn len(&self) -> usize {
        self.meta.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn deserialize_impl(serialized: &PyBytes) -> PyResult<Self> {
        let data = serialized.as_bytes();
        bincode::deserialize(data).map_err(|e| {
            let msg = format!("could not deserialize FrameStackHandle: {e:?}");
            PyRuntimeError::new_err(msg)
        })
    }

    fn get_slice_for_frame<'a>(&'a self, frame_idx: usize, slot: &'a Slot) -> &[u8] {
        let slice = slot.as_slice();
        let in_offset = frame_idx * self.bytes_per_frame;
        &slice[in_offset..in_offset + self.bytes_per_frame]
    }

    /// Split self at `mid` and create two new `FrameStackHandle`s.
    /// The first will contain frames `0..mid`, the second `mid..`
    pub fn split_at(self, mid: usize, shm: &mut SharedSlabAllocator) -> (Self, Self) {
        // FIXME: this whole thing is falliable, so modify return type to Result<> (or PyResult<>?)
        let (left, right) = {
            let slot: Slot = shm.get(self.slot.slot_idx);
            let slice = slot.as_slice();

            let mut slot_left = shm.get_mut().expect("shm slot for writing");
            let slice_left = slot_left.as_slice_mut();

            let mut slot_right = shm.get_mut().expect("shm slot for writing");
            let slice_right = slot_right.as_slice_mut();

            for i in 0..(mid * self.bytes_per_frame) {
                slice_left[i] = slice[i];
            }
            for i in (mid * self.bytes_per_frame)..slice.len() {
                slice_right[i - (mid * self.bytes_per_frame)] = slice[i];
            }

            let left = shm.writing_done(slot_left);
            let right = shm.writing_done(slot_right);

            shm.free_idx(self.slot.slot_idx);

            (left, right)
        };

        let (left_meta, right_meta) = self.meta.split_at(mid);

        (
            FrameStackHandle {
                slot: left,
                meta: left_meta.to_vec(),
                bytes_per_frame: self.bytes_per_frame,
            },
            FrameStackHandle {
                slot: right,
                meta: right_meta.to_vec(),
                bytes_per_frame: self.bytes_per_frame,
            },
        )
    }

    fn first_meta(&self) -> PyResult<&FrameMeta> {
        self.meta.first().map_or_else(
            || Err(PyValueError::new_err("empty frame stack".to_string())),
            Ok,
        )
    }
}

#[pymethods]
impl FrameStackHandle {
    fn serialize(&self, py: Python) -> PyResult<Py<PyBytes>> {
        let bytes: &PyBytes = PyBytes::new(py, serialize(self).unwrap().as_slice());
        Ok(bytes.into())
    }

    #[classmethod]
    fn deserialize(_cls: &PyType, serialized: &PyBytes) -> PyResult<Self> {
        Self::deserialize_impl(serialized)
    }

    fn get_series_id(slf: PyRef<Self>) -> PyResult<u64> {
        Ok(slf.first_meta()?.dimage.series)
    }

    fn get_frame_id(slf: PyRef<Self>) -> PyResult<u64> {
        Ok(slf.first_meta()?.dimage.frame)
    }

    fn get_hash(slf: PyRef<Self>) -> PyResult<String> {
        Ok(slf.first_meta()?.dimage.hash.clone())
    }

    fn get_pixel_type(slf: PyRef<Self>) -> PyResult<String> {
        Ok(match &slf.first_meta()?.dimaged.type_ {
            PixelType::Uint8 => "uint8".to_string(),
            PixelType::Uint16 => "uint16".to_string(),
            PixelType::Uint32 => "uint32".to_string(),
        })
    }

    fn get_encoding(slf: PyRef<Self>) -> PyResult<String> {
        Ok(slf.first_meta()?.dimaged.encoding.clone())
    }

    /// return endianess in numpy notation
    fn get_endianess(slf: PyRef<Self>) -> PyResult<String> {
        match slf.first_meta()?.dimaged.encoding.chars().last() {
            Some(c) => Ok(c.to_string()),
            None => Err(exceptions::PyValueError::new_err(
                "encoding should be non-empty".to_string(),
            )),
        }
    }

    fn get_shape(slf: PyRef<Self>) -> PyResult<Vec<u64>> {
        Ok(slf.first_meta()?.dimaged.shape.clone())
    }

    fn __len__(slf: PyRef<Self>) -> usize {
        slf.len()
    }
}

#[pyclass]
pub struct CamClient {
    shm: Option<SharedSlabAllocator>,
}

fn handle_connection(mut stream: UnixStream, handle: SHMHandle) {
    let fds = [handle.fd];
    let info = bincode::serialize(&handle.info).expect("serialize shm info");

    stream
        .write_all(&info.len().to_be_bytes())
        .expect("send shm info size");
    stream
        .send_with_fd(&info, &fds)
        .expect("send shm info with fds");
}

/// start a thread that serves shm handles at the given socket path
pub fn serve_shm_handle(handle: SHMHandle, socket_path: &str) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let path = Path::new(socket_path);

    let stop_event = Arc::new(AtomicBool::new(false));

    if path.exists() {
        remove_file(path).expect("remove existing socket");
    }

    let listener = UnixListener::bind(socket_path).unwrap();

    let outer_stop = Arc::clone(&stop_event);

    listener
        .set_nonblocking(true)
        .expect("set to nonblocking accept");

    let join_handle = std::thread::spawn(move || {
        // Stolen from the example on `UnixListener`:
        // accept connections and process them, spawning a new thread for each one

        loop {
            if stop_event.load(Ordering::Relaxed) {
                debug!("stopping `serve_shm_handle` thread");
                break;
            }
            let stream = listener.accept();
            match stream {
                Ok((stream, _addr)) => {
                    /* connection succeeded */
                    std::thread::spawn(move || handle_connection(stream, handle));
                }
                Err(err) => {
                    /* EAGAIN / EWOULDBLOCK */
                    if err.kind() == io::ErrorKind::WouldBlock {
                        let fd = listener.as_raw_fd();
                        let pollfd = PollFd::new(fd, PollFlags::all());
                        nix::poll::poll(&mut [pollfd], 100).expect("poll for socket to be ready");
                        continue;
                    }
                    /* connection failed */
                    log::error!("connection failed: {err}");
                    break;
                }
            }
        }
    });

    (outer_stop, join_handle)
}

fn read_size(mut stream: &UnixStream) -> usize {
    let mut buf: [u8; std::mem::size_of::<usize>()] = [0; std::mem::size_of::<usize>()];
    stream.read_exact(&mut buf).expect("read message size");
    usize::from_be_bytes(buf)
}

/// connect to the given unix domain socket and grab a SHM handle
fn recv_shm_handle(socket_path: &str) -> SHMHandle {
    let stream = UnixStream::connect(socket_path).expect("connect to socket");

    let mut fds: [i32; 1] = [0];

    let size = read_size(&stream);
    let mut bytes: Vec<u8> = vec![0; size];

    stream
        .recv_with_fd(bytes.as_mut_slice(), &mut fds)
        .expect("read initial message with fds");

    let info: SHMInfo = bincode::deserialize(&bytes[..]).expect("deserialize SHMInfo object");

    SHMHandle { fd: fds[0], info }
}

impl CamClient {
    fn decompress_frame_stack_impl<T: numpy::Element>(
        &self,
        handle: &FrameStackHandle,
        out: &PyArray3<T>,
    ) -> PyResult<()> {
        let mut out_rw = out.readwrite();
        let out_slice = out_rw.as_slice_mut().expect("`out` must be C-contiguous");
        let slot: Slot = if let Some(shm) = &self.shm {
            shm.get(handle.slot.slot_idx)
        } else {
            return Err(PyRuntimeError::new_err("can't decompress with closed SHM"));
        };

        for (frame_meta, idx) in handle.meta.iter().zip(0..) {
            let out_size = usize::try_from(frame_meta.get_size()).unwrap();

            // FIXME: broken if frames in a stack have different sizes
            let out_offset = idx * out_size;
            let out_ptr: *mut T = out_slice[out_offset..out_offset + out_size]
                .as_mut_ptr()
                .cast();

            let image_data =
                &handle.get_slice_for_frame(idx, &slot)[..frame_meta.data_length_bytes];

            match decompress_lz4_into(&image_data[12..], out_ptr, out_size, None) {
                Ok(()) => {}
                Err(e) => {
                    let msg = format!("decompression failed: {e:?}");
                    return Err(DecompressError::new_err(msg));
                }
            }
        }

        Ok(())
    }
}

#[pymethods]
impl CamClient {
    #[new]
    fn new(socket_path: &str) -> PyResult<Self> {
        let handle = recv_shm_handle(socket_path);
        match SharedSlabAllocator::connect(handle.fd, &handle.info) {
            Ok(shm) => Ok(CamClient { shm: Some(shm) }),
            Err(e) => {
                let msg = format!("failed to connect to SHM: {:?}", e);
                Err(ConnectionError::new_err(msg))
            }
        }
    }

    fn decompress_frame_stack(
        slf: PyRef<Self>,
        handle: &FrameStackHandle,
        out: &PyAny,
    ) -> PyResult<()> {
        let arr_u8: Result<&PyArray3<u8>, _> = out.downcast();
        let arr_u16: Result<&PyArray3<u16>, _> = out.downcast();
        let arr_u32: Result<&PyArray3<u32>, _> = out.downcast();

        let encoding = if handle.is_empty() {
            return Ok(());
        } else {
            &handle.meta.first().unwrap().dimaged.encoding
        };

        match encoding.as_str() {
            "bs32-lz4<" => {
                slf.decompress_frame_stack_impl(handle, arr_u32.unwrap())?;
            }
            "bs16-lz4<" => {
                slf.decompress_frame_stack_impl(handle, arr_u16.unwrap())?;
            }
            "bs8-lz4<" => {
                slf.decompress_frame_stack_impl(handle, arr_u8.unwrap())?;
            }
            e => {
                let msg = format!("can't deal with encoding {e}");
                return Err(exceptions::PyValueError::new_err(msg));
            }
        }
        Ok(())
    }

    fn done(mut slf: PyRefMut<Self>, handle: &FrameStackHandle) -> PyResult<()> {
        let slot_idx = handle.slot.slot_idx;
        if let Some(shm) = &mut slf.shm {
            Ok(shm.free_idx(slot_idx))
        } else {
            return Err(PyRuntimeError::new_err(
                "CamClient.done called with SHM closed",
            ));
        }
    }

    fn close(&mut self) {
        self.shm.take();
    }
}

impl Drop for CamClient {
    fn drop(&mut self) {
        trace!("CamClient::drop");
    }
}

#[cfg(test)]
mod tests {
    use ipc_test::{SharedSlabAllocator, Slot};
    use numpy::PyArray;
    use pyo3::{prepare_freethreaded_python, Python};
    use tempfile::tempdir;

    use crate::{
        bs::compress_lz4,
        shm_recv::{serve_shm_handle, CamClient, FrameStackHandle},
    };

    use super::FrameStackForWriting;

    #[test]
    fn test_frame_stack() {
        let mut shm = SharedSlabAllocator::new(1, 4096, false).unwrap();
        let slot = shm.get_mut().expect("get a free shm slot");
        let mut fs = FrameStackForWriting::new(slot, 1, 256);
        let dimage = crate::common::DImage {
            htype: "".to_string(),
            series: 1,
            frame: 1,
            hash: "".to_string(),
        };
        let dimaged = crate::common::DImageD {
            htype: "".to_string(),
            shape: vec![512, 512],
            type_: crate::common::PixelType::Uint16,
            encoding: ">bslz4".to_string(),
        };
        let dconfig = crate::common::DConfig {
            htype: "".to_string(),
            start_time: 0,
            stop_time: 0,
            real_time: 0,
        };
        fs.frame_done(dimage, dimaged, dconfig, &[42]);

        let _fs_handle = fs.writing_done(&mut shm);
    }

    #[test]
    fn test_cam_client() {
        let mut shm = SharedSlabAllocator::new(1, 4096, false).unwrap();
        let slot = shm.get_mut().expect("get a free shm slot");
        let mut fs = FrameStackForWriting::new(slot, 1, 512);
        let dimage = crate::common::DImage {
            htype: "".to_string(),
            series: 1,
            frame: 1,
            hash: "".to_string(),
        };
        let dimaged = crate::common::DImageD {
            htype: "".to_string(),
            shape: vec![16, 16],
            type_: crate::common::PixelType::Uint16,
            encoding: "bs16-lz4<".to_string(),
        };
        let dconfig = crate::common::DConfig {
            htype: "".to_string(),
            start_time: 0,
            stop_time: 0,
            real_time: 0,
        };

        // some predictable test data:
        let in_: Vec<u16> = (0..256).map(|i| i % 16).collect();
        let compressed_data = compress_lz4(&in_, None).unwrap();

        // compressed dectris data stream has an (unknown)
        // header in front of the compressed data, which we just cut off,
        // so here we just prepend 12 zero-bytes
        let mut data_with_prefix = vec![0; 12];
        data_with_prefix.extend_from_slice(&compressed_data);
        assert!(data_with_prefix.len() < 512);
        data_with_prefix.iter().take(12).for_each(|&e| {
            assert_eq!(e, 0);
        });
        println!("{:x?}", &compressed_data);
        println!("{:x?}", &data_with_prefix[12..]);
        fs.frame_done(dimage, dimaged, dconfig, &data_with_prefix);

        // we have one frame in there:
        assert_eq!(fs.len(), 1);

        let fs_handle = fs.writing_done(&mut shm);

        // we still have one frame in there:
        assert_eq!(fs_handle.len(), 1);

        // initialize a Python interpreter so we are able to construct a PyBytes instance:
        prepare_freethreaded_python();

        // roundtrip serialize/deserialize:
        Python::with_gil(|py| {
            let bytes = fs_handle.serialize(py).unwrap();
            let new_handle = FrameStackHandle::deserialize_impl(bytes.as_ref(py)).unwrap();
            assert_eq!(fs_handle, new_handle);
        });

        // start to serve the shm connection via a unix domain socket:
        let handle = shm.get_handle();
        let socket_dir = tempdir().unwrap();
        let socket_as_path = socket_dir.into_path().join("stuff.socket");
        let socket_path = socket_as_path.to_string_lossy();
        serve_shm_handle(handle, &socket_path);

        let client = CamClient::new(&socket_path).unwrap();

        let slot_r: Slot = shm.get(fs_handle.slot.slot_idx);
        let slice = slot_r.as_slice();
        println!("{:x?}", slice);

        Python::with_gil(|py| {
            let flat: Vec<u16> = (0..256).collect();
            let out = PyArray::from_vec(py, flat).reshape((1, 16, 16)).unwrap();
            client.decompress_frame_stack_impl(&fs_handle, out).unwrap();

            out.readonly()
                .as_slice()
                .unwrap()
                .iter()
                .zip(0..)
                .for_each(|(&item, idx)| {
                    assert_eq!(item, in_[idx]);
                    assert_eq!(item, (idx % 16) as u16);
                });
        });
    }

    #[test]
    fn test_split_frame_stack_handle() {
        // need at least three slots: one is the source, two for the results.
        let mut shm = SharedSlabAllocator::new(3, 4096, false).unwrap();
        let slot = shm.get_mut().expect("get a free shm slot");
        let mut fs = FrameStackForWriting::new(slot, 2, 16);
        let dimage = crate::common::DImage {
            htype: "".to_string(),
            series: 1,
            frame: 1,
            hash: "".to_string(),
        };
        let dimaged = crate::common::DImageD {
            htype: "".to_string(),
            shape: vec![512, 512],
            type_: crate::common::PixelType::Uint16,
            encoding: ">bslz4".to_string(),
        };
        let dconfig = crate::common::DConfig {
            htype: "".to_string(),
            start_time: 0,
            stop_time: 0,
            real_time: 0,
        };
        fs.frame_done(
            dimage.clone(),
            dimaged.clone(),
            dconfig.clone(),
            &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
        );
        fs.frame_done(
            dimage.clone(),
            dimaged.clone(),
            dconfig.clone(),
            &[2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
        );

        println!("{:?}", fs.slot.as_slice());

        let fs_handle = fs.writing_done(&mut shm);

        let old_meta_len = fs_handle.meta.len();

        let (a, b) = fs_handle.split_at(1, &mut shm);

        let slot_a: Slot = shm.get(a.slot.slot_idx);
        let slot_b: Slot = shm.get(b.slot.slot_idx);
        let slice_a = &slot_a.as_slice()[..16];
        let slice_b = &slot_b.as_slice()[..16];
        println!("{:?}", slice_a);
        println!("{:?}", slice_b);
        for &elem in slice_a {
            assert_eq!(elem, 1);
        }
        for &elem in slice_b {
            assert_eq!(elem, 2);
        }

        assert_eq!(a.meta.len() + b.meta.len(), old_meta_len);

        // when the split is done, there should be one free shm slot:
        assert_eq!(shm.num_slots_free(), 1);

        // and we can free them again:
        shm.free_idx(a.slot.slot_idx);
        shm.free_idx(b.slot.slot_idx);

        assert_eq!(shm.num_slots_free(), 3);
    }
}
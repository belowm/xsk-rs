use libbpf_sys::{xsk_ring_cons, xsk_ring_prod, xsk_umem, xsk_umem_config};
use std::{cmp, collections::VecDeque, convert::TryInto, io, mem::MaybeUninit, ptr};

use crate::{
    poll::{poll_read, Milliseconds},
    socket::Fd,
};

mod config;
mod mmap;

use config::Config;
use mmap::MmapArea;

pub struct FrameDesc {
    addr: u64,
    len: u32,
    options: u32,
}

impl FrameDesc {
    pub(crate) fn new(addr: u64, len: u32, options: u32) -> Self {
        FrameDesc { addr, len, options }
    }

    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn options(&self) -> u32 {
        self.options
    }
}

pub struct UmemBuilder {
    config: Config,
}

pub struct UmemBuilderWithMmap {
    config: Config,
    mmap_area: MmapArea,
}

pub struct Umem {
    inner: Box<xsk_umem>,
    mmap_area: MmapArea,
    frame_descs: Option<Vec<FrameDesc>>,
    frame_count: u32,
    frame_size: u32,
}

pub enum UmemError {
    InvalidFrameAddr,
    InvalidDataLen,
}

pub struct CompQueue {
    inner: Box<xsk_ring_cons>,
}

pub struct FillQueue {
    inner: Box<xsk_ring_prod>,
}

impl UmemBuilder {
    pub fn create_mmap(self) -> io::Result<UmemBuilderWithMmap> {
        let mmap_area = MmapArea::new(self.config.umem_len(), self.config.use_huge_pages())?;

        Ok(UmemBuilderWithMmap {
            config: self.config,
            mmap_area,
        })
    }
}

impl UmemBuilderWithMmap {
    pub fn create_umem(mut self) -> io::Result<(Umem, FillQueue, CompQueue)> {
        let umem_create_config = xsk_umem_config {
            fill_size: self.config.fill_queue_size(),
            comp_size: self.config.comp_queue_size(),
            frame_size: self.config.frame_size(),
            frame_headroom: self.config.frame_headroom(),
            flags: self.config.umem_flags().bits(),
        };

        let mut umem_ptr: *mut xsk_umem = ptr::null_mut();
        let mut fq_ptr: MaybeUninit<xsk_ring_prod> = MaybeUninit::uninit();
        let mut cq_ptr: MaybeUninit<xsk_ring_cons> = MaybeUninit::uninit();

        let err = unsafe {
            libbpf_sys::xsk_umem__create(
                &mut umem_ptr,
                self.mmap_area.as_mut_ptr(),
                self.mmap_area.len,
                fq_ptr.as_mut_ptr(),
                cq_ptr.as_mut_ptr(),
                &umem_create_config,
            )
        };

        if err != 0 {
            return Err(io::Error::from_raw_os_error(err));
        }

        // Assuming 64-bit architecture so casting from u32 to u64 should be ok
        let mut frame_descs: Vec<FrameDesc> =
            Vec::with_capacity(self.config.frame_count().try_into().unwrap());

        // Assuming 64-bit architecture so casting from u32 to u64 in 'addr' should be ok
        // Also know from UmemBuilder that i * frame_size won't overflow, as max val is
        // frame_count * frame_size
        for i in 0..self.config.frame_count() {
            let addr = (i * self.config.frame_size()).try_into().unwrap();
            let len = 0;
            let options = 0;

            let frame_desc = FrameDesc { addr, len, options };

            frame_descs.push(frame_desc);
        }

        let umem = Umem {
            inner: unsafe { Box::from_raw(umem_ptr) },
            mmap_area: self.mmap_area,
            frame_descs: Some(frame_descs),
            frame_count: self.config.frame_count(),
            frame_size: self.config.frame_size(),
        };

        let fill_queue = FillQueue {
            inner: unsafe { Box::new(fq_ptr.assume_init()) },
        };

        let comp_queue = CompQueue {
            inner: unsafe { Box::new(cq_ptr.assume_init()) },
        };

        Ok((umem, fill_queue, comp_queue))
    }
}

impl Umem {
    pub fn new(config: Config) -> UmemBuilder {
        UmemBuilder { config }
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut xsk_umem {
        self.inner.as_mut()
    }

    pub fn frame_ref(&self, addr: u64) -> Result<&[u8], UmemError> {
        // First check that frame address and frame length are within bounds
        if addr > ((self.frame_count - 1).try_into().unwrap()) {
            return Err(UmemError::InvalidFrameAddr);
        }

        unsafe {
            let frame_ptr = self.mmap_area.as_ptr().offset(addr.try_into().unwrap());

            Ok(std::slice::from_raw_parts(
                frame_ptr as *const u8,
                self.frame_size.try_into().unwrap(),
            ))
        }
    }

    pub fn frame_ref_mut(&mut self, addr: u64) -> Result<&mut [u8], UmemError> {
        if addr > ((self.frame_count - 1).try_into().unwrap()) {
            return Err(UmemError::InvalidFrameAddr);
        }

        unsafe {
            let frame_ptr = self.mmap_area.as_mut_ptr().offset(addr.try_into().unwrap());

            Ok(std::slice::from_raw_parts_mut(
                frame_ptr as *mut u8,
                self.frame_size.try_into().unwrap(),
            ))
        }
    }

    pub fn copy_data_to_frame(&mut self, addr: u64, data: &[u8]) -> Result<(), UmemError> {
        if data.len() > self.frame_size.try_into().unwrap() {
            return Err(UmemError::InvalidDataLen);
        }

        let frame_ref = self.frame_ref_mut(addr)?;

        frame_ref[..data.len()].copy_from_slice(data);

        Ok(())
    }

    pub fn consume_frame_descs(&mut self) -> Option<Vec<FrameDesc>> {
        self.frame_descs.take()
    }
}

impl Drop for Umem {
    fn drop(&mut self) {
        let err = unsafe { libbpf_sys::xsk_umem__delete(self.inner.as_mut()) };

        if err != 0 {
            eprintln!("xsk_umem__delete failed: {}", err);
        }
    }
}

impl FillQueue {
    pub fn produce(&mut self, descs: &mut VecDeque<FrameDesc>, nb: u64) -> u64 {
        if nb == 0 {
            return 0;
        }

        // First determine how many slots are free. Need to do this because if we try to reserve
        // more than is available in 'xsk_ring_prod__reserve' it will fail
        let nb_free: u64 = unsafe { libbpf_sys::_xsk_prod_nb_free(self.inner.as_mut(), 0) }
            .try_into()
            .unwrap();

        // Assuming 64-bit architecture so usize -> u64 / u32 -> u64 should be fine
        let nb = cmp::min(nb_free, cmp::min(nb, descs.len().try_into().unwrap()));

        if nb == 0 {
            return 0;
        }

        let mut idx: u32 = 0;

        let cnt = unsafe { libbpf_sys::_xsk_ring_prod__reserve(self.inner.as_mut(), nb, &mut idx) };

        for _ in 0..cnt {
            // Ensured above that cnt <= descs.len()
            let desc = descs.pop_front().unwrap();

            unsafe {
                *libbpf_sys::_xsk_ring_prod__fill_addr(self.inner.as_mut(), idx) = desc.addr;
            }
            idx += 1;
        }

        if cnt > 0 {
            unsafe { libbpf_sys::_xsk_ring_prod__submit(self.inner.as_mut(), cnt) };
        }

        cnt
    }

    pub fn produce_and_wakeup(
        &mut self,
        descs: &mut VecDeque<FrameDesc>,
        nb: u64,
        socket_fd: &Fd,
        poll_timeout: &Milliseconds,
    ) -> io::Result<u64> {
        let cnt = self.produce(descs, nb);

        if cnt > 0 && self.needs_wakeup() {
            poll_read(socket_fd, poll_timeout)?;
        }

        Ok(cnt)
    }

    fn needs_wakeup(&self) -> bool {
        unsafe {
            if libbpf_sys::_xsk_ring_prod__needs_wakeup(self.inner.as_ref()) != 0 {
                true
            } else {
                false
            }
        }
    }
}

impl CompQueue {
    pub fn consume(&mut self, descs: &mut VecDeque<FrameDesc>, nb: u64) -> u64 {
        if nb == 0 {
            return 0;
        }

        let mut idx: u32 = 0;

        let cnt = unsafe { libbpf_sys::_xsk_ring_cons__peek(self.inner.as_mut(), nb, &mut idx) };

        for _ in 0..cnt {
            let addr = unsafe { *libbpf_sys::_xsk_ring_cons__comp_addr(self.inner.as_mut(), idx) };

            let desc = FrameDesc::new(addr, 0, 0);

            descs.push_back(desc);

            idx += 1;
        }

        if cnt > 0 {
            unsafe { libbpf_sys::_xsk_ring_cons__release(self.inner.as_mut(), cnt) };
        }

        cnt
    }
}

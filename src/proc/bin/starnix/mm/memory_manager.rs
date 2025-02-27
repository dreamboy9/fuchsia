// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon::{self as zx, AsHandleRef, HandleBased};
use lazy_static::lazy_static;
use parking_lot::{Mutex, RwLock};
use process_builder::elf_load;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;
use zerocopy::{AsBytes, FromBytes};

use crate::collections::*;
use crate::logging::*;
use crate::types::*;

lazy_static! {
    pub static ref PAGE_SIZE: u64 = zx::system_get_page_size() as u64;
}

#[derive(Debug, Eq, PartialEq, Clone)]
struct Mapping {
    /// The base address of this mapping.
    ///
    /// Keep in mind that the mapping might be trimmed in the RangeMap if the
    /// part of the mapping is unmapped, which means the base might extend
    /// before the currently valid portion of the mapping.
    base: UserAddress,

    /// The VMO that contains the memory used in this mapping.
    vmo: Arc<zx::Vmo>,

    /// The offset in the VMO that corresponds to the base address.
    vmo_offset: u64,

    /// The rights used by the mapping.
    permissions: zx::VmarFlags,

    /// A name associated with the mapping. Set by prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, ...).
    name: CString,
}

impl Mapping {
    fn new(base: UserAddress, vmo: Arc<zx::Vmo>, vmo_offset: u64, flags: zx::VmarFlags) -> Mapping {
        Mapping {
            base,
            vmo,
            vmo_offset,
            permissions: flags
                & (zx::VmarFlags::PERM_READ
                    | zx::VmarFlags::PERM_WRITE
                    | zx::VmarFlags::PERM_EXECUTE),
            name: CString::default(),
        }
    }

    fn with_flags(&self, flags: zx::VmarFlags) -> Mapping {
        Mapping {
            base: self.base,
            vmo: self.vmo.clone(),
            vmo_offset: self.vmo_offset,
            permissions: flags
                & (zx::VmarFlags::PERM_READ
                    | zx::VmarFlags::PERM_WRITE
                    | zx::VmarFlags::PERM_EXECUTE),
            name: self.name.clone(),
        }
    }
}

const PROGRAM_BREAK_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
struct ProgramBreak {
    // These base address at which the data segment is mapped.
    base: UserAddress,

    // The current program break.
    //
    // The addresses from [base, current.round_up(*PAGE_SIZE)) are mapped into the
    // client address space from the underlying |vmo|.
    current: UserAddress,
}

/// The policy about whether the address space can be dumped.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DumpPolicy {
    /// The address space cannot be dumped.
    ///
    /// Corresponds to SUID_DUMP_DISABLE.
    DISABLE,

    /// The address space can be dumped.
    ///
    /// Corresponds to SUID_DUMP_USER.
    USER,
}

struct MemoryManagerState {
    /// The VMAR in which userspace mappings occur.
    ///
    /// We map userspace memory in this child VMAR so that we can destroy the
    /// entire VMAR during exec.
    user_vmar: zx::Vmar,

    /// State for the brk and sbrk syscalls.
    brk: Option<ProgramBreak>,

    /// The memory mappings currently used by this address space.
    ///
    /// The mappings record which VMO backs each address.
    mappings: RangeMap<UserAddress, Mapping>,
}

impl MemoryManagerState {
    fn map(
        &mut self,
        vmar_offset: usize,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        length: usize,
        flags: zx::VmarFlags,
    ) -> Result<UserAddress, zx::Status> {
        let addr = UserAddress::from_ptr(self.user_vmar.map(
            vmar_offset,
            &vmo,
            vmo_offset,
            length,
            flags,
        )?);
        let mapping = Mapping::new(addr, vmo, vmo_offset, flags);
        let end = (addr + length).round_up(*PAGE_SIZE);
        self.mappings.insert(addr..end, mapping);
        Ok(addr)
    }

    fn unmap(&mut self, addr: UserAddress, length: usize) -> Result<(), Errno> {
        // This operation is safe because we're operating on another process.
        match unsafe { self.user_vmar.unmap(addr.ptr(), length) } {
            Ok(_) => Ok(()),
            Err(zx::Status::NOT_FOUND) => Ok(()),
            Err(zx::Status::INVALID_ARGS) => Err(EINVAL),
            Err(status) => Err(impossible_error(status)),
        }?;
        let end = (addr + length).round_up(*PAGE_SIZE);
        self.mappings.remove(&(addr..end));
        Ok(())
    }

    pub fn protect(
        &mut self,
        addr: UserAddress,
        length: usize,
        flags: zx::VmarFlags,
    ) -> Result<(), Errno> {
        let (_, mapping) = self.mappings.get(&addr).ok_or(EINVAL)?;
        let mapping = mapping.with_flags(flags);

        // SAFETY: This is safe because the vmar belongs to a different process.
        unsafe { self.user_vmar.protect(addr.ptr(), length, flags) }.map_err(|s| match s {
            zx::Status::INVALID_ARGS => EINVAL,
            // TODO: This should still succeed and change protection on whatever is mapped.
            zx::Status::NOT_FOUND => EINVAL,
            zx::Status::ACCESS_DENIED => EACCES,
            _ => impossible_error(s),
        })?;

        let end = (addr + length).round_up(*PAGE_SIZE);
        self.mappings.insert(addr..end, mapping);
        Ok(())
    }
}

fn create_user_vmar(vmar: &zx::Vmar, vmar_info: &zx::VmarInfo) -> Result<zx::Vmar, zx::Status> {
    let (vmar, ptr) = vmar.allocate(
        0,
        vmar_info.len,
        zx::VmarFlags::SPECIFIC
            | zx::VmarFlags::CAN_MAP_SPECIFIC
            | zx::VmarFlags::CAN_MAP_READ
            | zx::VmarFlags::CAN_MAP_WRITE
            | zx::VmarFlags::CAN_MAP_EXECUTE,
    )?;
    assert_eq!(ptr, vmar_info.base);
    Ok(vmar)
}

pub struct MemoryManager {
    /// A handle to the underlying Zircon process object.
    // TODO: Remove this handle once we can read and write process memory directly.
    process: zx::Process,

    /// The root VMAR for the child process.
    ///
    /// Instead of mapping memory directly in this VMAR, we map the memory in
    /// `state.user_vmar`.
    root_vmar: zx::Vmar,

    /// The base address of the root_vmar.
    pub base_addr: UserAddress,

    /// Mutable state for the memory manager.
    state: RwLock<MemoryManagerState>,

    /// Whether this address space is dumpable.
    pub dumpable: Mutex<DumpPolicy>,
}

impl MemoryManager {
    pub fn new(process: zx::Process, root_vmar: zx::Vmar) -> Result<Self, zx::Status> {
        let info = root_vmar.info()?;
        let user_vmar = create_user_vmar(&root_vmar, &info)?;
        Ok(MemoryManager {
            process,
            root_vmar,
            base_addr: UserAddress::from_ptr(info.base),
            state: RwLock::new(MemoryManagerState {
                user_vmar,
                brk: None,
                mappings: RangeMap::new(),
            }),
            dumpable: Mutex::new(DumpPolicy::DISABLE),
        })
    }

    pub fn set_brk(&self, addr: UserAddress) -> Result<UserAddress, Errno> {
        let mut state = self.state.write();

        // Ensure that a program break exists by mapping at least one page.
        let mut brk = match state.brk {
            None => {
                let vmo = zx::Vmo::create(PROGRAM_BREAK_LIMIT).map_err(|_| ENOMEM)?;
                vmo.set_name(CStr::from_bytes_with_nul(b"starnix-brk\0").unwrap())
                    .map_err(impossible_error)?;
                let length = *PAGE_SIZE as usize;
                let addr = state
                    .map(
                        0,
                        Arc::new(vmo),
                        0,
                        length,
                        zx::VmarFlags::PERM_READ
                            | zx::VmarFlags::PERM_WRITE
                            | zx::VmarFlags::REQUIRE_NON_RESIZABLE,
                    )
                    .map_err(Self::get_errno_for_map_err)?;
                let brk = ProgramBreak { base: addr, current: addr };
                state.brk = Some(brk);
                brk
            }
            Some(brk) => brk,
        };

        if addr < brk.base || addr > brk.base + PROGRAM_BREAK_LIMIT {
            // The requested program break is out-of-range. We're supposed to simply
            // return the current program break.
            return Ok(brk.current);
        }

        let (range, mapping) = state.mappings.get(&brk.current).ok_or(EFAULT)?;

        brk.current = addr;

        let old_end = range.end;
        let new_end = (brk.current + 1u64).round_up(*PAGE_SIZE);

        if new_end < old_end {
            // We've been asked to free memory.
            let delta = old_end - new_end;
            let vmo = mapping.vmo.clone();
            state.unmap(new_end, delta)?;
            let vmo_offset = new_end - brk.base;
            vmo.op_range(zx::VmoOp::DECOMMIT, vmo_offset as u64, delta as u64)
                .map_err(|e| impossible_error(e))?;
        } else if new_end > old_end {
            // We've been asked to map more memory.
            let delta = new_end - old_end;
            let vmo_offset = old_end - brk.base;
            let range = range.clone();
            let mapping = mapping.clone();

            state.mappings.remove(&range);
            match state.user_vmar.map(
                old_end - self.base_addr,
                &mapping.vmo,
                vmo_offset as u64,
                delta,
                zx::VmarFlags::PERM_READ
                    | zx::VmarFlags::PERM_WRITE
                    | zx::VmarFlags::REQUIRE_NON_RESIZABLE
                    | zx::VmarFlags::SPECIFIC,
            ) {
                Ok(_) => {
                    state.mappings.insert(brk.base..new_end, mapping);
                }
                Err(e) => {
                    // We failed to extend the mapping, which means we need to add
                    // back the old mapping.
                    state.mappings.insert(brk.base..old_end, mapping);
                    return Err(Self::get_errno_for_map_err(e));
                }
            }
        }

        state.brk = Some(brk);
        return Ok(brk.current);
    }

    pub fn snapshot_to(&self, target: &MemoryManager) -> Result<(), Errno> {
        let state = self.state.read();
        let mut target_state = target.state.write();

        let mut vmos = HashMap::<zx::Koid, Result<Arc<zx::Vmo>, zx::Status>>::new();

        for (range, mapping) in state.mappings.iter() {
            let vmo_info = mapping.vmo.info().map_err(impossible_error)?;
            let entry = vmos.entry(vmo_info.koid).or_insert_with(|| {
                if vmo_info.flags.contains(zx::VmoInfoFlags::PAGER_BACKED) {
                    Ok(mapping.vmo.clone())
                } else {
                    mapping
                        .vmo
                        .create_child(zx::VmoChildOptions::SNAPSHOT, 0, vmo_info.size_bytes)
                        .map(|vmo| Arc::new(vmo))
                }
            });
            match entry {
                Ok(target_vmo) => {
                    let vmo_offset = mapping.vmo_offset + (range.start - mapping.base) as u64;
                    let length = range.end - range.start;
                    target_state
                        .map(
                            range.start - target.base_addr,
                            target_vmo.clone(),
                            vmo_offset,
                            length,
                            mapping.permissions | zx::VmarFlags::SPECIFIC,
                        )
                        .map_err(Self::get_errno_for_map_err)?;
                }
                Err(_) => {
                    return Err(ENOMEM);
                }
            };
        }

        target_state.brk = state.brk;
        *target.dumpable.lock() = *self.dumpable.lock();

        Ok(())
    }

    pub fn exec(&self) -> Result<(), zx::Status> {
        let mut state = self.state.write();
        let info = self.root_vmar.info()?;
        // SAFETY: This operation is safe because the VMAR is for another process.
        unsafe { state.user_vmar.destroy()? }
        state.user_vmar = create_user_vmar(&self.root_vmar, &info)?;
        state.brk = None;
        state.mappings = RangeMap::new();

        *self.dumpable.lock() = DumpPolicy::DISABLE;
        Ok(())
    }

    fn get_errno_for_map_err(status: zx::Status) -> Errno {
        match status {
            zx::Status::INVALID_ARGS => EINVAL,
            zx::Status::ACCESS_DENIED => EACCES, // or EPERM?
            zx::Status::NOT_SUPPORTED => ENODEV,
            zx::Status::NO_MEMORY => ENOMEM,
            _ => impossible_error(status),
        }
    }

    pub fn map(
        &self,
        addr: UserAddress,
        vmo: zx::Vmo,
        vmo_offset: u64,
        length: usize,
        flags: zx::VmarFlags,
    ) -> Result<UserAddress, Errno> {
        let vmar_offset = if addr.is_null() { 0 } else { addr - self.base_addr };
        let mut state = self.state.write();
        state
            .map(vmar_offset, Arc::new(vmo), vmo_offset, length, flags)
            .map_err(Self::get_errno_for_map_err)
    }

    pub fn unmap(&self, addr: UserAddress, length: usize) -> Result<(), Errno> {
        let mut state = self.state.write();
        state.unmap(addr, length)
    }

    pub fn protect(
        &self,
        addr: UserAddress,
        length: usize,
        flags: zx::VmarFlags,
    ) -> Result<(), Errno> {
        let mut state = self.state.write();
        state.protect(addr, length, flags)
    }

    pub fn set_mapping_name(
        &self,
        addr: UserAddress,
        length: usize,
        name: CString,
    ) -> Result<(), Errno> {
        let mut state = self.state.write();
        let (range, mapping) = state.mappings.get_mut(&addr).ok_or(EINVAL)?;
        if range.end - addr < length {
            return Err(EINVAL);
        }
        let _result = mapping.vmo.set_name(&name);
        mapping.name = name;
        Ok(())
    }

    #[cfg(test)]
    pub fn get_mapping_name(&self, addr: UserAddress) -> Result<CString, Errno> {
        let state = self.state.read();
        let (_, mapping) = state.mappings.get(&addr).ok_or(EFAULT)?;
        Ok(mapping.name.clone())
    }

    #[cfg(test)]
    pub fn get_mapping_count(&self) -> usize {
        let state = self.state.read();
        state.mappings.iter().count()
    }

    pub fn get_random_base(&self, length: usize) -> UserAddress {
        let state = self.state.read();
        // Allocate a vmar of the correct size, get the random location, then immediately destroy it.
        // This randomizes the load address without loading into a sub-vmar and breaking mprotect.
        // This is different from how Linux actually lays out the address space. We might need to
        // rewrite it eventually.
        let (temp_vmar, base) =
            state.user_vmar.allocate(0, length, zx::VmarFlags::empty()).unwrap();
        // SAFETY: This is safe because the vmar is not in the current process.
        unsafe { temp_vmar.destroy().unwrap() };
        UserAddress::from_ptr(base)
    }

    pub fn read_memory(&self, addr: UserAddress, bytes: &mut [u8]) -> Result<(), Errno> {
        let actual = self.process.read_memory(addr.ptr(), bytes).map_err(|_| EFAULT)?;
        if actual != bytes.len() {
            return Err(EFAULT);
        }
        Ok(())
    }

    pub fn read_object<T: AsBytes + FromBytes>(
        &self,
        user: UserRef<T>,
        object: &mut T,
    ) -> Result<(), Errno> {
        self.read_memory(user.addr(), object.as_bytes_mut())
    }

    pub fn read_c_string<'a>(
        &self,
        string: UserCString,
        buffer: &'a mut [u8],
    ) -> Result<&'a [u8], Errno> {
        let actual = self.process.read_memory(string.ptr(), buffer).map_err(|_| EFAULT)?;
        let buffer = &mut buffer[..actual];
        let null_index = memchr::memchr(b'\0', buffer).ok_or(ENAMETOOLONG)?;
        Ok(&buffer[..null_index])
    }

    pub fn write_memory(&self, addr: UserAddress, bytes: &[u8]) -> Result<(), Errno> {
        let actual = self.process.write_memory(addr.ptr(), bytes).map_err(|_| EFAULT)?;
        if actual != bytes.len() {
            return Err(EFAULT);
        }
        Ok(())
    }

    pub fn write_object<T: AsBytes + FromBytes>(
        &self,
        user: UserRef<T>,
        object: &T,
    ) -> Result<(), Errno> {
        self.write_memory(user.addr(), &object.as_bytes())
    }
}

impl elf_load::Mapper for MemoryManager {
    fn map(
        &self,
        vmar_offset: usize,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        length: usize,
        flags: zx::VmarFlags,
    ) -> Result<usize, zx::Status> {
        let vmo = Arc::new(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?);
        let mut state = self.state.write();
        state.map(vmar_offset, vmo, vmo_offset, length, flags).map(|addr| addr.ptr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;

    use crate::syscalls::*;
    use crate::testing::*;

    #[fasync::run_singlethreaded(test)]
    async fn test_brk() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let mm = &task_owner.task.mm;

        // Look up the given addr in the mappings table.
        let get_range = |addr: &UserAddress| {
            let state = mm.state.read();
            let (range, _) = state.mappings.get(&addr).expect("failed to find mapping");
            range.clone()
        };

        // Initialize the program break.
        let base_addr =
            mm.set_brk(UserAddress::default()).expect("failed to set initial program break");
        assert!(base_addr > UserAddress::default());

        // Check that the initial program break actually maps some memory.
        let range0 = get_range(&base_addr);
        assert_eq!(range0.start, base_addr);
        assert_eq!(range0.end, base_addr + *PAGE_SIZE);

        // Grow the program break by a tiny amount that does not actually result in a change.
        let addr1 = mm.set_brk(base_addr + 1u64).expect("failed to grow brk");
        assert_eq!(addr1, base_addr + 1u64);
        let range1 = get_range(&base_addr);
        assert_eq!(range1.start, range0.start);
        assert_eq!(range1.end, range0.end);

        // Grow the program break by a non-trival amount and observe the larger mapping.
        let addr2 = mm.set_brk(base_addr + 24893u64).expect("failed to grow brk");
        assert_eq!(addr2, base_addr + 24893u64);
        let range2 = get_range(&base_addr);
        assert_eq!(range2.start, base_addr);
        assert_eq!(range2.end, addr2.round_up(*PAGE_SIZE));

        // Shrink the program break and observe the smaller mapping.
        let addr3 = mm.set_brk(base_addr + 14832u64).expect("failed to shrink brk");
        assert_eq!(addr3, base_addr + 14832u64);
        let range3 = get_range(&base_addr);
        assert_eq!(range3.start, base_addr);
        assert_eq!(range3.end, addr3.round_up(*PAGE_SIZE));

        // Shrink the program break close to zero and observe the smaller mapping.
        let addr4 = mm.set_brk(base_addr + 3u64).expect("failed to drastically shrink brk");
        assert_eq!(addr4, base_addr + 3u64);
        let range4 = get_range(&base_addr);
        assert_eq!(range4.start, base_addr);
        assert_eq!(range4.end, addr4.round_up(*PAGE_SIZE));

        // Shrink the program break close to zero and observe that the mapping is not entirely gone.
        let addr5 = mm.set_brk(base_addr).expect("failed to drastically shrink brk to zero");
        assert_eq!(addr5, base_addr);
        let range5 = get_range(&base_addr);
        assert_eq!(range5.start, base_addr);
        assert_eq!(range5.end, addr5 + *PAGE_SIZE);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_mm_exec() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);
        let mm = &task_owner.task.mm;

        let has = |addr: &UserAddress| -> bool {
            let state = mm.state.read();
            state.mappings.get(addr).is_some()
        };

        let brk_addr =
            mm.set_brk(UserAddress::default()).expect("failed to set initial program break");
        assert!(brk_addr > UserAddress::default());
        assert!(has(&brk_addr));

        let mapped_addr = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert!(mapped_addr > UserAddress::default());
        assert!(has(&mapped_addr));

        mm.exec().expect("failed to exec memory manager");

        assert!(!has(&brk_addr));
        assert!(!has(&mapped_addr));

        // Check that the old addresses are actually available for mapping.
        let brk_addr2 = map_memory(&ctx, brk_addr, *PAGE_SIZE);
        assert_eq!(brk_addr, brk_addr2);
        let mapped_addr2 = map_memory(&ctx, mapped_addr, *PAGE_SIZE);
        assert_eq!(mapped_addr, mapped_addr2);
    }
}

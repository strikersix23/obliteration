pub use self::mem::*;
pub use self::module::*;
use self::resolver::{ResolveFlags, SymbolResolver};
use crate::budget::ProcType;
use crate::ee::native::{NativeEngine, SetupModuleError};
use crate::errno::{Errno, EINVAL, ENOENT, ENOEXEC, ENOMEM, EPERM, ESRCH};
use crate::fs::VFileFlags;
use crate::fs::{Fs, OpenError, VPath, VPathBuf};
use crate::idt::Entry;
use crate::imgact::orbis::{DynamicFlags, Elf, FileType, ReadProgramError, Relocation, Symbol};
use crate::info;
use crate::log::print;
use crate::process::{Binaries, VThread};
use crate::syscalls::{SysErr, SysIn, SysOut, Syscalls};
use crate::vm::{MemoryUpdateError, MmapError, Protections};
use bitflags::bitflags;
use gmtx::{Gutex, GutexGroup, GutexWriteGuard};
use macros::Errno;
use sha1::{Digest, Sha1};
use std::borrow::Cow;
use std::io::Write;
use std::mem::{size_of, zeroed};
use std::num::NonZeroI32;
use std::ops::Deref;
use std::ptr::{read_unaligned, write_unaligned};
use std::sync::Arc;
use thiserror::Error;

mod mem;
mod module;
mod resolver;

/// An implementation of
/// https://github.com/freebsd/freebsd-src/blob/release/9.1.0/libexec/rtld-elf/rtld.c.
#[derive(Debug)]
pub struct RuntimeLinker {
    fs: Arc<Fs>,
    ee: Arc<NativeEngine>,
    // TODO: Move all fields after this to proc.
    kernel: Gutex<Option<Arc<Module>>>, // obj_kernel
    tls: Gutex<TlsAlloc>,
    flags: Gutex<LinkerFlags>,
}

impl RuntimeLinker {
    const NID_CHARS: &'static [u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";
    const NID_SALT: [u8; 16] = [
        0x51, 0x8d, 0x64, 0xa6, 0x35, 0xde, 0xd8, 0xc1, 0xe6, 0xb0, 0x39, 0xb1, 0xc3, 0xe5, 0x52,
        0x30,
    ];

    pub fn new(fs: &Arc<Fs>, ee: &Arc<NativeEngine>, sys: &mut Syscalls) -> Arc<Self> {
        let gg = GutexGroup::new();
        let ld = Arc::new(Self {
            fs: fs.clone(),
            ee: ee.clone(),
            kernel: gg.spawn(None),
            tls: gg.spawn(TlsAlloc {
                max_index: 1,
                last_offset: 0,
                last_size: 0,
                static_space: 0,
            }),
            flags: gg.spawn(LinkerFlags::empty()),
        });

        sys.register(591, &ld, Self::sys_dynlib_dlsym);
        sys.register(592, &ld, Self::sys_dynlib_get_list);
        sys.register(593, &ld, Self::sys_dynlib_get_info);
        sys.register(594, &ld, Self::sys_dynlib_load_prx);
        sys.register(596, &ld, Self::sys_dynlib_do_copy_relocations);
        sys.register(598, &ld, Self::sys_dynlib_get_proc_param);
        sys.register(599, &ld, Self::sys_dynlib_process_needed_and_relocate);
        sys.register(608, &ld, Self::sys_dynlib_get_info_ex);
        sys.register(649, &ld, Self::sys_dynlib_get_obj_member);

        ld
    }

    pub fn kernel(&self) -> Option<Arc<Module>> {
        self.kernel.read().clone()
    }

    pub fn set_kernel(&self, md: Arc<Module>) {
        *self.kernel.write() = Some(md);
    }

    /// See `kern_execve` on the PS4 for a reference.
    pub fn exec(&self, path: impl AsRef<VPath>, td: &VThread) -> Result<Arc<Module>, ExecError> {
        // Open executable.
        let path = path.as_ref();
        let file = self
            .fs
            .open(path, VFileFlags::READ, Some(td))
            .map_err(ExecError::OpenExeFailed)?;
        let elf = Elf::open(path.as_str(), file).map_err(ExecError::ReadExeFailed)?;

        // Check image type.
        match elf.ty() {
            FileType::ET_EXEC | FileType::ET_SCE_EXEC | FileType::ET_SCE_REPLAY_EXEC => {
                if elf.info().is_none() {
                    todo!("a statically linked eboot.bin is not supported yet.");
                }
            }
            FileType::ET_SCE_DYNEXEC if elf.dynamic().is_some() => {}
            _ => return Err(ExecError::InvalidExe),
        }

        // Get base address.
        let base = if elf.ty() == FileType::ET_SCE_DYNEXEC {
            0x400000
        } else {
            0
        };

        // TODO: Check exec_new_vmspace on the PS4 to see what we have missed here.
        // TODO: Apply remaining checks from exec_self_imgact.
        // Map eboot.bin.
        let mut app = Module::map(
            &self.ee,
            td.proc(),
            elf,
            base,
            "executable",
            0,
            Vec::new(),
            1,
        )
        .map_err(ExecError::MapFailed)?;

        *app.flags_mut() |= ModuleFlags::MAINPROG;

        self.ee
            .setup_module(&mut app)
            .map_err(ExecError::SetupFailed)?;

        // Check if application need certain modules.
        let mut flags = LinkerFlags::empty();

        for m in app.modules() {
            match m.name() {
                "libSceDbgUndefinedBehaviorSanitizer" => flags |= LinkerFlags::HAS_UBSAN,
                "libSceDbgAddressSanitizer" => flags |= LinkerFlags::HAS_ASAN,
                _ => continue,
            }
        }

        // TODO: Move modules preload to here for dynamic executable.
        // TODO: Apply logic from dmem_handle_process_exec_begin.
        // TODO: Apply logic from procexec_handler.
        // TODO: Apply logic from umtx_exec_hook.
        // TODO: Apply logic from aio_proc_rundown_exec.
        // TODO: Apply logic from gs_is_event_handler_process_exec.
        let app = Arc::new(app);
        let mut bin = td.proc().bin_mut();

        if bin.is_some() {
            todo!("multiple exec on the same process");
        }

        *bin = Some(Binaries::new(app.clone()));

        *self.flags.write() = flags;

        Ok(app)
    }

    /// See `load_object`, `do_load_object` and `self_load_shared_object` on the PS4 for a
    /// reference.
    pub fn load<'a>(
        &self,
        path: impl AsRef<VPath>,
        _: LoadFlags,
        force: bool,
        main: bool,
        td: &'a VThread,
    ) -> Result<(Arc<Module>, GutexWriteGuard<'a, Binaries>), LoadError> {
        // Check if module with the same path already loaded.
        let path = path.as_ref();
        let mut bin = td.proc().bin_mut().ok_or(LoadError::NoExe)?;
        let loaded = bin.list().skip(1).find(|m| m.path() == path);

        if let Some(v) = loaded {
            *v.ref_count_mut() += 1;

            return Ok((v.clone(), bin));
        }

        // Check if module with the same base name already loaded.
        let name = path.file_name().unwrap().to_owned();

        if !force {
            let loaded = bin.list().skip(1).find(|m| m.names().contains(&name));

            if let Some(v) = loaded {
                return Ok((v.clone(), bin));
            }
        }

        // Check if application is decid.(s)elf.
        let app = bin.app().path().file_name().unwrap();

        if app != "decid.elf" && app != "decid.self" {
            // TODO: Check what the PS4 is doing here.
        }

        if self.flags.read().intersects(LinkerFlags::HAS_ASAN) {
            todo!("do_load_object with sanitizer & 2");
        }

        // Get file.
        let file = match self.fs.open(path, VFileFlags::READ, Some(td)) {
            Ok(v) => v,
            Err(e) => return Err(LoadError::OpenFileFailed(e)),
        };

        // Load (S)ELF.
        let elf = match Elf::open(path, file) {
            Ok(v) => v,
            Err(e) => return Err(LoadError::OpenElfFailed(e)),
        };

        // Check image type.
        if elf.ty() != FileType::ET_SCE_DYNAMIC {
            return Err(LoadError::InvalidElf);
        }

        // TODO: Apply remaining checks from self_load_shared_object.
        // Search for TLS free slot.
        let names = vec![name];
        let tls = elf.tls().map(|i| &elf.programs()[i]);
        let tls = if tls.map_or(0, |p| p.memory_size()) == 0 {
            0
        } else {
            let mut alloc = self.tls.write();
            let mut index = 1;

            loop {
                // Check if the current value has been used.
                if !bin.list().any(|m| m.tls_index() == index) {
                    break;
                }

                // Someone already use the current value, increase the value and try again.
                index += 1;

                if index > alloc.max_index {
                    alloc.max_index = index;
                    break;
                }
            }

            index
        };

        // Map file.
        let mut table = td.proc().objects_mut();
        let (entry, _) = table.try_alloc_with(|id| {
            let name = path.file_name().unwrap();
            let id: u32 = (id + 1).try_into().unwrap();
            let mut md = match Module::map(&self.ee, td.proc(), elf, 0, name, id, names, tls) {
                Ok(v) => v,
                Err(e) => return Err(LoadError::MapFailed(e)),
            };

            if md.flags().contains(ModuleFlags::TEXT_REL) {
                return Err(LoadError::ImpureText);
            }

            // TODO: Check the call to sceSblAuthMgrIsLoadable in the self_load_shared_object on the PS4
            // to see how it is return the value.
            if name != "libc.sprx" && name != "libSceFios2.sprx" {
                *md.flags_mut() |= ModuleFlags::IS_SYSTEM;
            }

            if let Err(e) = self.ee.setup_module(&mut md) {
                return Err(LoadError::SetupFailed(e));
            }

            Ok(Entry::new(None, Arc::new(md), 0x2000))
        })?;

        // Add to list.
        let module = entry.data().clone().downcast::<Module>().unwrap();

        bin.push(module.clone(), main);

        Ok((module, bin))
    }

    /// See `init_dag` on the PS4 for a reference.
    fn init_dag(&self, md: &Arc<Module>) {
        // Do nothing if already initializes.
        let mut flags = md.flags_mut();

        if flags.intersects(ModuleFlags::DAG_INITED) {
            return;
        }

        // Add the module itself as a first member of DAG.
        md.dag_static_mut().push(md.clone());
        md.dag_dynamic_mut().push(md.clone());

        // TODO: Apply the remaining logics from init_dag.
        *flags |= ModuleFlags::DAG_INITED;
    }

    /// See `do_dlsym` on the PS4 for a reference.
    fn resolve_symbol<'a>(
        &self,
        bin: &Binaries,
        md: &'a Arc<Module>,
        mut name: Cow<'a, str>,
        mut lib: Option<&'a str>,
        flags: ResolveFlags,
    ) -> Option<usize> {
        let mut mname = md.modules().iter().find(|i| i.id() == 0).map(|i| i.name());

        if flags.intersects(ResolveFlags::UNK1) {
            lib = None;
            mname = None;
        } else {
            if lib.is_none() {
                lib = mname;
            }
            name = Cow::Owned(Self::get_nid(name.as_ref()));
        }

        // Setup resolver.
        let resolver = SymbolResolver::new(
            bin,
            bin.app().sdk_ver() >= 0x5000000 || self.flags.read().contains(LinkerFlags::HAS_ASAN),
        );

        // Resolve.
        let dags = md.dag_static();

        let sym = if md.flags().intersects(ModuleFlags::MAINPROG) {
            todo!("do_dlsym on MAIN_PROG");
        } else {
            resolver.resolve_from_list(
                md,
                Some(name.as_ref()),
                None,
                mname,
                lib,
                SymbolResolver::hash(Some(name.as_ref()), lib, mname),
                flags | ResolveFlags::UNK3 | ResolveFlags::UNK4,
                dags.deref(),
            )
        };

        sym.map(|(m, s)| m.memory().addr() + m.memory().base() + m.symbol(s).unwrap().value())
    }

    fn get_nid(name: &str) -> String {
        // Get hash.
        let mut sha1 = Sha1::new();

        sha1.update(name.as_bytes());
        sha1.update(Self::NID_SALT);

        // Get NID.
        let hash = u64::from_ne_bytes(sha1.finalize()[..8].try_into().unwrap());
        let mut nid = vec![0; 11];

        nid[0] = Self::NID_CHARS[(hash >> 58) as usize];
        nid[1] = Self::NID_CHARS[(hash >> 52 & 0x3f) as usize];
        nid[2] = Self::NID_CHARS[(hash >> 46 & 0x3f) as usize];
        nid[3] = Self::NID_CHARS[(hash >> 40 & 0x3f) as usize];
        nid[4] = Self::NID_CHARS[(hash >> 34 & 0x3f) as usize];
        nid[5] = Self::NID_CHARS[(hash >> 28 & 0x3f) as usize];
        nid[6] = Self::NID_CHARS[(hash >> 22 & 0x3f) as usize];
        nid[7] = Self::NID_CHARS[(hash >> 16 & 0x3f) as usize];
        nid[8] = Self::NID_CHARS[(hash >> 10 & 0x3f) as usize];
        nid[9] = Self::NID_CHARS[(hash >> 4 & 0x3f) as usize];
        nid[10] = Self::NID_CHARS[((hash & 0xf) * 4) as usize];

        // SAFETY: This is safe because NID_CHARS is a valid UTF-8.
        unsafe { String::from_utf8_unchecked(nid) }
    }

    fn sys_dynlib_dlsym(self: &Arc<Self>, td: &Arc<VThread>, i: &SysIn) -> Result<SysOut, SysErr> {
        // Check if application is dynamic linking.
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EPERM))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EPERM));
        }

        // Get arguments.
        let handle: u32 = i.args[0].try_into().unwrap();
        let name = unsafe { i.args[1].to_str(2560)?.unwrap() };
        let out: *mut usize = i.args[2].into();

        // Get target module.
        let md = match bin.list().find(|m| m.id() == handle) {
            Some(v) => v,
            None => return Err(SysErr::Raw(ESRCH)),
        };

        info!("Getting symbol '{}' from {}.", name, md.path());

        // Get resolving flags.
        let mut flags = ResolveFlags::UNK1;

        if name != "BaOKcng8g88" && name != "KpDMrPHvt3Q" {
            flags = ResolveFlags::empty();
        }

        // Resolve the symbol.
        let addr = match self.resolve_symbol(&bin, md, name.into(), None, flags) {
            Some(v) => v,
            None => return Err(SysErr::Raw(ESRCH)),
        };

        unsafe { *out = addr };
        Ok(SysOut::ZERO)
    }

    fn sys_dynlib_get_list(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        // Get arguments.
        let buf: *mut u32 = i.args[0].into();
        let max: usize = i.args[1].into();
        let copied: *mut usize = i.args[2].into();

        // Check if application is dynamic linking.
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EPERM))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EPERM));
        }

        // Copy module ID.
        let list = bin.list();
        let len = list.len();

        if len > max {
            return Err(SysErr::Raw(ENOMEM));
        }

        for (i, m) in list.enumerate() {
            unsafe { *buf.add(i) = m.id() };
        }

        // Set copied.
        unsafe { *copied = len };

        Ok(SysOut::ZERO)
    }

    fn sys_dynlib_get_info(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        let handle: u32 = i.args[0].try_into().unwrap();
        let info = {
            let info_out: *mut DynlibInfo = i.args[1].into();

            unsafe { &mut *info_out }
        };

        info!("Getting info for module id = {}.", handle);

        let bin = td.proc().bin();

        let bin = match bin.as_ref() {
            Some(bin) if bin.app().file_info().is_some() => bin,
            _ => return Err(SysErr::Raw(EPERM)),
        };

        if info.size != size_of::<DynlibInfo>() {
            return Err(SysErr::Raw(EINVAL));
        }

        *info = self.dynlib_get_info(handle, bin, true)?;

        Ok(SysOut::ZERO)
    }

    fn dynlib_get_info(
        &self,
        handle: u32,
        bin: &Binaries,
        no_system: bool,
    ) -> Result<DynlibInfo, SysErr> {
        let mut info: DynlibInfo = unsafe { zeroed() };

        let mut modules = bin.list();

        let md = modules
            .find(|m| m.id() == handle)
            .ok_or(SysErr::Raw(ESRCH))?;

        if no_system && md.flags().intersects(ModuleFlags::IS_SYSTEM) {
            return Err(SysErr::Raw(EPERM));
        }

        let name = md.path().file_name().unwrap();

        let mem = md.memory();
        let addr = mem.addr();

        info.name[..name.len()].copy_from_slice(name.as_bytes());
        info.name[0xff] = 0;
        info.segment_count = 2;

        info.text_segment.addr = mem.base();
        info.text_segment.size = mem.text_segment().len().try_into().unwrap();
        info.text_segment.prot = 5;

        info.data_segment.addr = addr + mem.data_segment().start();
        info.data_segment.size = mem.data_segment().len().try_into().unwrap();
        info.data_segment.prot = 3;

        if let Some(seg) = mem.relro_segment() {
            info.relro_segment.addr = addr + seg.start();
            info.relro_segment.size = seg.len().try_into().unwrap();
            info.relro_segment.prot = 1;

            info.segment_count += 1;
        };

        info.fingerprint = md.fingerprint();

        let mut e = info!();

        writeln!(
            e,
            "Retrieved info for module {} (ID = {}).",
            md.path(),
            handle
        )
        .unwrap();

        writeln!(e, "mapbase : {:#x}", info.text_segment.addr).unwrap();
        writeln!(e, "textsize: {:#x}", info.text_segment.size).unwrap();
        writeln!(e, "database: {:#x}", info.data_segment.addr).unwrap();
        writeln!(e, "datasize: {:#x}", info.data_segment.size).unwrap();

        print(e);

        Ok(info)
    }

    fn sys_dynlib_load_prx(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        // Not sure what is this. Maybe kernel only flags?
        let mut flags: u32 = i.args[1].try_into().unwrap();

        if (flags & 0xfff8ffff) != 0 {
            return Err(SysErr::Raw(EINVAL));
        }

        // TODO: It looks like the PS4 check if this get called from a browser. The problem is this
        // check has been patched when jailbreaking so we need to see the original code before
        // implement this.
        let name = match VPath::new(unsafe { i.args[0].to_str(1024)?.unwrap() }) {
            Some(v) => v,
            None => todo!("sys_dynlib_load_prx with relative path"),
        };

        if td.proc().budget_ptype() == ProcType::BigApp {
            flags |= 0x01;
        }

        info!("Loading {name} with {flags:#x}.");

        // TODO: Refactor this for readability.
        let (md, mut bin) = self.load(
            name,
            LoadFlags::from_bits_retain(((flags & 1) << 5) + ((flags >> 10) & 0x40) + 2),
            true, // TODO: This hard-coded because we don't support relative path yet.
            false,
            td,
        )?;

        // Add to global list if it is not in the list yet.
        if !bin.globals().any(|m| Arc::ptr_eq(m, &md)) {
            bin.push_global(md.clone());
        }

        // The PS4 checking on the refcount to see if it need to do relocation. We can't do the same
        // here because we get this value from Arc, which is not the same as PS4.
        let mut mf = md.flags_mut();

        if !mf.intersects(ModuleFlags::DAG_INITED) {
            // TODO: Refactor this for readability.
            let mut v1 = mf.bits();
            let mut v2 = v1 | 0x800;

            if (flags & 0x20000) == 0 {
                v2 = v1 & 0xf7ff;
            }

            v1 = v2 | 0x1000;

            if (flags & 0x40000) == 0 {
                v1 = v2 & 0xefff;
            }

            *mf = ModuleFlags::from_bits_retain(v1);
            drop(mf); // init_dag need to lock this.

            // Initialize DAG and relocate the module.
            let resolver = SymbolResolver::new(
                &bin,
                bin.app().sdk_ver() >= 0x5000000
                    || self.flags.read().contains(LinkerFlags::HAS_ASAN),
            );

            self.init_dag(&md);

            if unsafe { self.relocate(&md, bin.list(), &resolver).is_err() } {
                todo!("sys_dynlib_load_prx with location failed");
            }
        }

        drop(bin);

        // Print the module.
        let mut log = info!();
        writeln!(log, "Module {} is loaded with ID = {}.", name, md.id()).unwrap();
        md.print(log);

        // Set module ID.
        unsafe { *Into::<*mut u32>::into(i.args[2]) = md.id() };

        // TODO: Apply the remaining logics from the PS4.
        Ok(SysOut::ZERO)
    }

    fn sys_dynlib_do_copy_relocations(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        _: &SysIn,
    ) -> Result<SysOut, SysErr> {
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EPERM))?;

        if let Some(info) = bin.app().file_info() {
            if info.relocs().any(|r| r.ty() == Relocation::R_X86_64_COPY) {
                return Err(SysErr::Raw(EINVAL));
            }

            Ok(SysOut::ZERO)
        } else {
            Err(SysErr::Raw(EPERM))
        }
    }

    fn sys_dynlib_get_proc_param(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        // Get arguments.
        let param: *mut usize = i.args[0].into();
        let size: *mut usize = i.args[1].into();

        // Check if application is a dynamic SELF.
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EPERM))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EPERM));
        }

        // Get param.
        match bin.app().proc_param() {
            Some((param_offset, param_size)) => {
                // TODO: Seems like ET_SCE_DYNEXEC is mapped at a fixed address.
                unsafe { *param = bin.app().memory().addr() + *param_offset };
                unsafe { *size = *param_size };
            }
            None => todo!("app is dynamic but no PT_SCE_PROCPARAM"),
        }

        Ok(SysOut::ZERO)
    }

    fn sys_dynlib_process_needed_and_relocate(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        _: &SysIn,
    ) -> Result<SysOut, SysErr> {
        // Check if application is dynamic linking.
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EINVAL))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EINVAL));
        }

        // Initialize module DAG.
        for md in bin.list() {
            self.init_dag(md);
        }

        // Initialize TLS.
        let mut tls = self.tls.write();

        for md in bin.mains() {
            // Skip if already initialized.
            let mut flags = md.flags_mut();

            if flags.contains(ModuleFlags::TLS_DONE) {
                continue;
            }

            // Check if the module has TLS.
            if let Some(t) = md.tls_info().filter(|i| i.size() != 0) {
                // TODO: Refactor this for readability.
                let off = if md.tls_index() == 1 {
                    (t.size() + t.align() - 1) & !(t.align() - 1)
                } else {
                    ((tls.last_offset + t.size()) + t.align() - 1) & !(t.align() - 1)
                };

                if tls.static_space != 0 && off > tls.static_space {
                    continue;
                }

                *md.tls_offset_mut() = off;

                tls.last_offset = off;
                tls.last_size = t.size();
            }

            // Set TLS_DONE.
            *flags |= ModuleFlags::TLS_DONE;
        }

        drop(tls);

        // Do relocation.
        let resolver = SymbolResolver::new(
            bin,
            bin.app().sdk_ver() >= 0x5000000 || self.flags.read().contains(LinkerFlags::HAS_ASAN),
        );

        info!("Relocating initial modules.");

        unsafe { self.relocate(bin.app(), bin.list(), &resolver) }?;

        // TODO: Apply the remaining logics from the PS4.
        Ok(SysOut::ZERO)
    }

    /// See `relocate_objects` on the PS4 for a reference.
    ///
    /// # Safety
    /// No other threads may access the memory of all loaded modules.
    unsafe fn relocate<'a>(
        &self,
        md: &Arc<Module>,
        list: impl Iterator<Item = &'a Arc<Module>>,
        resolver: &SymbolResolver,
    ) -> Result<(), RelocateError> {
        // TODO: Implement flags & 0x800.
        self.relocate_single(md, resolver)?;

        // Relocate other modules.
        for m in list {
            if Arc::ptr_eq(m, md) {
                continue;
            }

            self.relocate_single(m, resolver)?;
        }

        Ok(())
    }

    /// See `relocate_one_object` on the PS4 kernel for a reference.
    ///
    /// # Safety
    /// No other thread may access the module memory.
    unsafe fn relocate_single<'b>(
        &self,
        md: &'b Arc<Module>,
        resolver: &SymbolResolver<'b>,
    ) -> Result<(), RelocateError> {
        // Unprotect the memory.
        let mut mem = match md.memory().unprotect() {
            Ok(v) => v,
            Err(e) => return Err(RelocateError::UnprotectFailed(md.path().to_owned(), e)),
        };

        // Apply relocations.
        let mut relocated = md.relocated_mut();

        self.relocate_rela(md, mem.as_mut(), &mut relocated, resolver)?;

        if !md.flags().contains(ModuleFlags::JMPSLOTS_DONE) {
            self.relocate_plt(md, mem.as_mut(), &mut relocated, resolver)?;
        }

        Ok(())
    }

    /// See `reloc_non_plt` on the PS4 kernel for a reference.
    fn relocate_rela<'b>(
        &self,
        md: &'b Arc<Module>,
        mem: &mut [u8],
        relocated: &mut [Option<Relocated>],
        resolver: &SymbolResolver<'b>,
    ) -> Result<(), RelocateError> {
        let info = md.file_info().unwrap(); // Let it panic because the PS4 assume it is available.
        let addr = mem.as_ptr() as usize;
        let base = md.memory().base();

        for (i, reloc) in info.relocs().enumerate() {
            // Check if the entry already relocated.
            if relocated[i].is_some() {
                continue;
            }

            // Resolve value.
            let offset = base + reloc.offset();
            let target = &mut mem[offset..(offset + 8)];
            let addend = reloc.addend();
            let sym = reloc.symbol();
            let symflags = ResolveFlags::empty();
            let (how, value) = match reloc.ty() {
                Relocation::R_X86_64_NONE => break,
                Relocation::R_X86_64_64 => {
                    // TODO: Apply checks from reloc_non_plt.
                    let (md, sym) = match resolver.resolve_with_local(md, sym, symflags) {
                        Some(v) => v,
                        None => continue,
                    };

                    // TODO: Apply checks from reloc_non_plt.
                    let (how, value) = Self::get_relocated(md, sym);

                    (how, value.wrapping_add_signed(addend))
                }
                Relocation::R_X86_64_GLOB_DAT => {
                    // TODO: Apply checks from reloc_non_plt.
                    let (md, sym) = match resolver.resolve_with_local(md, sym, symflags) {
                        Some(v) => v,
                        None => continue,
                    };

                    // TODO: Apply checks from reloc_non_plt.
                    Self::get_relocated(md, sym)
                }
                Relocation::R_X86_64_RELATIVE => {
                    // TODO: Apply checks from reloc_non_plt.
                    let addend: usize = addend.try_into().unwrap();
                    let offset = base + addend;
                    let seg = md
                        .memory()
                        .segments()
                        .iter()
                        .find(|&s| s.program().is_some() && offset >= s.start() && offset < s.end())
                        .unwrap();

                    if seg.prot().intersects(Protections::CPU_EXEC) {
                        let func = unsafe { md.get_function(addend) };
                        let value = func.addr();
                        (Relocated::Executable(func), value)
                    } else {
                        let value = addr + offset;
                        (Relocated::Data((md.clone(), value)), value)
                    }
                }
                Relocation::R_X86_64_DTPMOD64 => {
                    // TODO: Apply checks from reloc_non_plt.
                    let md = match resolver.resolve_with_local(md, sym, symflags) {
                        Some((md, _)) => md,
                        None => continue,
                    };

                    let index: usize = md.tls_index().try_into().unwrap();
                    let value = unsafe { read_unaligned::<usize>(target.as_ptr().cast()) + index };

                    (Relocated::Tls((md, index)), value)
                }
                Relocation::R_X86_64_DTPOFF64 => {
                    let md = match resolver.resolve_with_local(md, sym, symflags) {
                        Some((md, _)) => md,
                        None => continue,
                    };

                    let sym = md.symbol(sym).unwrap();
                    let value = unsafe { read_unaligned::<usize>(target.as_ptr().cast()) };

                    let relocated = (value + sym.value()).wrapping_add_signed(addend);

                    (Relocated::Data((md, relocated)), relocated)
                }
                v => return Err(RelocateError::UnsupportedRela(md.path().to_owned(), v)),
            };

            // TODO: Check what relocate_text_or_data_segment on the PS4 is doing.
            unsafe { write_unaligned(target.as_mut_ptr().cast(), value) };

            relocated[i] = Some(how);
        }

        Ok(())
    }

    /// See `reloc_jmplots` on the PS4 for a reference.
    fn relocate_plt<'b>(
        &self,
        md: &'b Arc<Module>,
        mem: &mut [u8],
        relocated: &mut [Option<Relocated>],
        resolver: &SymbolResolver<'b>,
    ) -> Result<(), RelocateError> {
        // Do nothing if not a dynamic module.
        let info = match md.file_info() {
            Some(v) => v,
            None => return Ok(()),
        };

        // Apply relocations.
        let base = md.memory().base();

        for (i, reloc) in info.plt_relocs().enumerate() {
            // Check if the entry already relocated.
            let index = info.reloc_count() + i;

            if relocated[index].is_some() {
                continue;
            }

            // Check relocation type.
            if reloc.ty() != Relocation::R_X86_64_JUMP_SLOT {
                return Err(RelocateError::UnsupportedPlt(
                    md.path().to_owned(),
                    reloc.ty(),
                ));
            }

            // Resolve symbol.
            let (md, sym) =
                match resolver.resolve_with_local(md, reloc.symbol(), ResolveFlags::UNK1) {
                    Some(v) => v,
                    None => continue,
                };

            // Write the value.
            let (how, value) = Self::get_relocated(md, sym);
            let offset = base + reloc.offset();
            let target = &mut mem[offset..(offset + 8)];
            let value = value.wrapping_add_signed(reloc.addend());

            unsafe { write_unaligned(target.as_mut_ptr().cast(), value) };

            relocated[index] = Some(how);
        }

        Ok(())
    }

    fn get_relocated(md: Arc<Module>, sym: usize) -> (Relocated, usize) {
        let sym = md.symbol(sym).unwrap();

        match sym.ty() {
            Symbol::STT_FUNC | Symbol::STT_ENTRY => {
                let func = unsafe { md.get_function(sym.value()) };
                let addr = func.addr();
                (Relocated::Executable(func), addr)
            }
            _ => {
                let mem = md.memory();
                let addr = mem.addr() + mem.base() + sym.value();
                (Relocated::Data((md, addr)), addr)
            }
        }
    }

    fn sys_dynlib_get_info_ex(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        // Get arguments.
        let handle: u32 = i.args[0].try_into().unwrap();
        let flags: u32 = i.args[1].try_into().unwrap();
        let info: *mut DynlibInfoEx = i.args[2].into();

        info!("Getting info_ex for module id = {}.", handle);

        // Check if application is dynamic linking.
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EPERM))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EPERM));
        }

        // Check buffer size.

        if unsafe { (*info).size } != size_of::<DynlibInfoEx>() {
            return Err(SysErr::Raw(EINVAL));
        }

        // Lookup the module.
        let md = match bin.list().find(|m| m.id() == handle) {
            Some(v) => v,
            None => return Err(SysErr::Raw(ESRCH)),
        };

        // Fill the info.
        let info = unsafe { &mut *info };
        let mem = md.memory();
        let addr = mem.addr();

        *info = unsafe { zeroed() };
        info.handle = md.id();

        info.text_segment.addr = addr + mem.base();
        info.text_segment.size = mem.text_segment().len().try_into().unwrap();
        info.text_segment.prot = 5;

        info.data_segment.addr = addr + mem.data_segment().start();
        info.data_segment.size = mem.data_segment().len().try_into().unwrap();
        info.data_segment.prot = 3;

        info.segment_count = 2;
        info.refcount = *md.ref_count();

        // Copy module name.
        if flags & 2 == 0 || !md.flags().contains(ModuleFlags::IS_SYSTEM) {
            let name = md.path().file_name().unwrap();

            info.name[..name.len()].copy_from_slice(name.as_bytes());
            info.name[0xff] = 0;
        }

        // Set TLS information. Not sure if the tlsinit can be zero when the tlsinitsize is zero.
        // Let's keep the same behavior as the PS4 for now.
        info.tlsindex = if flags & 1 != 0 {
            let flags = md.flags();
            let mut upper = if flags.contains(ModuleFlags::IS_SYSTEM) {
                1
            } else {
                0
            };

            if flags.contains(ModuleFlags::MAINPROG) {
                upper += 2;
            }

            (upper << 16) | (md.tls_index() & 0xffff)
        } else {
            md.tls_index() & 0xffff
        };

        if let Some(i) = md.tls_info() {
            info.tlsinit = addr + i.init();
            info.tlsinitsize = i.init_size().try_into().unwrap();
            info.tlssize = i.size().try_into().unwrap();
            info.tlsalign = i.align().try_into().unwrap();
        } else {
            info.tlsinit = addr;
        }

        info.tlsoffset = (*md.tls_offset()).try_into().unwrap();

        // Initialization and finalization functions.
        if !md.flags().contains(ModuleFlags::NOT_GET_PROC) {
            info.init = md.init().map(|v| addr + v).unwrap_or(0);
            info.fini = md.fini().map(|v| addr + v).unwrap_or(0);
        }

        // Exception handling.
        if let Some(i) = md.eh_info() {
            info.eh_frame_hdr = addr + i.header();
            info.eh_frame_hdr_size = i.header_size().try_into().unwrap();
            info.eh_frame = addr + i.frame();
            info.eh_frame_size = i.frame_size().try_into().unwrap();
        } else {
            info.eh_frame_hdr = addr;
        }

        let mut e = info!();

        writeln!(
            e,
            "Retrieved info_ex for module {} (ID = {}).",
            md.path(),
            handle
        )
        .unwrap();

        writeln!(e, "mapbase     : {:#x}", info.text_segment.addr).unwrap();
        writeln!(e, "textsize    : {:#x}", info.text_segment.size).unwrap();
        writeln!(e, "database    : {:#x}", info.data_segment.addr).unwrap();
        writeln!(e, "datasize    : {:#x}", info.data_segment.size).unwrap();
        writeln!(e, "tlsindex    : {}", info.tlsindex).unwrap();
        writeln!(e, "tlsinit     : {:#x}", info.tlsinit).unwrap();
        writeln!(e, "tlsoffset   : {:#x}", info.tlsoffset).unwrap();
        writeln!(e, "init        : {:#x}", info.init).unwrap();
        writeln!(e, "fini        : {:#x}", info.fini).unwrap();
        writeln!(e, "eh_frame_hdr: {:#x}", info.eh_frame_hdr).unwrap();

        print(e);

        Ok(SysOut::ZERO)
    }

    fn sys_dynlib_get_obj_member(
        self: &Arc<Self>,
        td: &Arc<VThread>,
        i: &SysIn,
    ) -> Result<SysOut, SysErr> {
        let handle: u32 = i.args[0].try_into().unwrap();
        let ty: u8 = i.args[1].try_into().unwrap();
        let out: *mut usize = i.args[2].into();
        let bin = td.proc().bin();
        let bin = bin.as_ref().ok_or(SysErr::Raw(EINVAL))?;

        if bin.app().file_info().is_none() {
            return Err(SysErr::Raw(EINVAL));
        }

        let module = bin
            .list()
            .find(|m| m.id() == handle)
            .ok_or(SysErr::Raw(ESRCH))?;

        unsafe {
            *out = match ty {
                1..=4 | 7 => todo!("sys_dynlib_get_obj_member: with ty = {ty}"),
                8 => module
                    .mod_param()
                    .map(|param| module.memory().addr() + param)
                    .expect("No mod param"),
                _ => return Err(SysErr::Raw(EINVAL)),
            }
        }

        Ok(SysOut::ZERO)
    }
}

#[derive(Debug)]
#[repr(C)]
struct SegmentInfo {
    addr: usize,
    size: u32,
    prot: u32,
}

#[derive(Debug)]
#[repr(C)]
struct DynlibInfo {
    size: usize,
    name: [u8; 256],
    text_segment: SegmentInfo,
    data_segment: SegmentInfo,
    relro_segment: SegmentInfo,
    unk_segment: SegmentInfo,
    segment_count: u32,
    fingerprint: [u8; 0x14],
}

const _: () = assert!(size_of::<DynlibInfo>() == 0x160);

#[derive(Debug)]
#[repr(C)]
struct DynlibInfoEx {
    size: usize,
    name: [u8; 256],
    handle: u32,
    tlsindex: u32,
    tlsinit: usize,
    tlsinitsize: u32,
    tlssize: u32,
    tlsoffset: u32,
    tlsalign: u32,
    init: usize,
    fini: usize,
    unk1: u64, // Always zero.
    unk2: u64, // Same here.
    eh_frame_hdr: usize,
    eh_frame: usize,
    eh_frame_hdr_size: u32,
    eh_frame_size: u32,
    text_segment: SegmentInfo,
    data_segment: SegmentInfo,
    relro_segment: SegmentInfo,
    unk_segment: SegmentInfo,
    segment_count: u32, // Always 2.
    refcount: u32,
}

const _: () = assert!(size_of::<DynlibInfoEx>() == 0x1a8);

/// Contains how TLS was allocated so far.
#[derive(Debug)]
pub struct TlsAlloc {
    max_index: u32,      // tls_max_index
    last_offset: usize,  // tls_last_offset
    last_size: usize,    // tls_last_size
    static_space: usize, // tls_static_space
}

bitflags! {
    /// Flags for [`RuntimeLinker`].
    #[derive(Debug)]
    pub struct LinkerFlags: u32 {
        const HAS_UBSAN = 0x01;
        const HAS_ASAN = 0x02;
    }
}

bitflags! {
    /// Flags for [`RuntimeLinker::load()`].
    #[derive(Clone, Copy)]
    pub struct LoadFlags: u32 {
        const UNK2 = 0x01;
        const BIG_APP = 0x20;
        const UNK1 = 0x40;
    }
}

/// Represents an error when [`RuntimeLinker::exec()`] fails.
#[derive(Debug, Error)]
pub enum ExecError {
    #[error("cannot open the executable")]
    OpenExeFailed(#[source] OpenError),

    #[error("cannot read the executable")]
    ReadExeFailed(#[source] crate::imgact::orbis::OpenError),

    #[error("invalid executable")]
    InvalidExe,

    #[error("cannot map the executable")]
    MapFailed(#[source] MapError),

    #[error("cannot setup the executable")]
    SetupFailed(#[source] SetupModuleError),
}

/// Represents an error for (S)ELF mapping.
#[derive(Debug, Error)]
pub enum MapError {
    #[error("the image has multiple executable programs")]
    MultipleExecProgram,

    #[error("the image has multiple data programs")]
    MultipleDataProgram,

    #[error("the image has multiple PT_SCE_RELRO")]
    MultipleRelroProgram,

    #[error("ELF program {0} has invalid alignment")]
    InvalidProgramAlignment(usize),

    #[error("cannot allocate {0} bytes")]
    MemoryAllocationFailed(usize, #[source] MmapError),

    #[error("cannot protect {1:#018x} bytes starting at {0:#x} with {2}")]
    ProtectMemoryFailed(usize, usize, Protections, #[source] MemoryUpdateError),

    #[error("cannot unprotect segment {0}")]
    UnprotectSegmentFailed(usize, #[source] UnprotectSegmentError),

    #[error("cannot read program #{0}")]
    ReadProgramFailed(usize, #[source] ReadProgramError),

    #[error("cannot unprotect the memory")]
    UnprotectMemoryFailed(#[source] UnprotectError),

    #[error("cannot read symbol entry {0}")]
    ReadSymbolFailed(usize, #[source] crate::imgact::orbis::ReadSymbolError),

    #[error("cannot read DT_NEEDED from dynamic entry {0}")]
    ReadNeededFailed(usize, #[source] crate::imgact::orbis::StringTableError),

    #[error("cannot read DT_SONAME from dynamic entry {0}")]
    ReadNameFailed(usize, #[source] crate::imgact::orbis::StringTableError),

    #[error("{0} is obsolete")]
    ObsoleteFlags(DynamicFlags),

    #[error("cannot read module info from dynamic entry {0}")]
    ReadModuleInfoFailed(usize, #[source] crate::imgact::orbis::ReadModuleError),

    #[error("cannot read libraru info from dynamic entry {0}")]
    ReadLibraryInfoFailed(usize, #[source] crate::imgact::orbis::ReadLibraryError),
}

/// Represents an error for (S)ELF loading.
#[derive(Debug, Error, Errno)]
pub enum LoadError {
    #[error("the process does not have executable file")]
    #[errno(EPERM)]
    NoExe,

    #[error("cannot open the specified file")]
    #[errno(ENOENT)]
    OpenFileFailed(#[source] OpenError),

    #[error("cannot open (S)ELF")]
    #[errno(ENOEXEC)]
    OpenElfFailed(#[source] crate::imgact::orbis::OpenError),

    #[error("the specified file is not valid module")]
    #[errno(ENOEXEC)]
    InvalidElf,

    #[error("cannot map file")]
    #[errno(ENOEXEC)]
    MapFailed(#[source] MapError),

    #[error("the specified file has impure text")]
    #[errno(EINVAL)]
    ImpureText,

    #[error("cannot setup the module")]
    #[errno(ENOEXEC)]
    SetupFailed(#[source] SetupModuleError),
}

/// Represents an error for modules relocation.
#[derive(Debug, Error)]
pub enum RelocateError {
    #[error("cannot unprotect the memory of {0}")]
    UnprotectFailed(VPathBuf, #[source] UnprotectError),

    #[error("relocation type {1} on {0} is not supported")]
    UnsupportedRela(VPathBuf, u32),

    #[error("PLT relocation type {1} on {0} is not supported")]
    UnsupportedPlt(VPathBuf, u32),
}

impl Errno for RelocateError {
    fn errno(&self) -> NonZeroI32 {
        match self {
            Self::UnprotectFailed(_, e) => match e {
                UnprotectError::MprotectFailed(_, _, _, _) => {
                    todo!("dynlib_process_needed_and_relocate with mprotect failed");
                }
            },
            Self::UnsupportedRela(_, _) => ENOEXEC,
            Self::UnsupportedPlt(_, _) => EINVAL,
        }
    }
}

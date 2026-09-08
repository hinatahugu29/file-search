//! Windows OLE ドラッグ＆ドロップ（DoDragDrop）連携モジュール。
//! 検索結果のアイテムを外部（エクスプローラーや他アプリ）へファイルとして直接ドラッグ＆ドロップできるようにする。

#[cfg(target_os = "windows")]
pub use windows_impl::{start_drag, start_drag_single};

#[cfg(not(target_os = "windows"))]
pub fn start_drag(_paths: &[&str]) {}

#[cfg(not(target_os = "windows"))]
pub fn start_drag_single(_path: &str) {}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Once;
    use windows::core::{implement, Result, HRESULT};
    use windows::Win32::Foundation::{
        DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL,
        POINT, S_OK,
    };
    use windows::Win32::System::Com::{
        IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, IAdviseSink,
        FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT,
        DROPEFFECT_COPY,
    };
    use windows::Win32::UI::Shell::DROPFILES;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_ESCAPE, VK_LBUTTON};

    static OLE_INIT: Once = Once::new();

    fn ensure_ole_init() {
        OLE_INIT.call_once(|| unsafe {
            let _ = OleInitialize(None);
        });
    }

    // CF_HDROP は Windows の定義済みクリップボード形式（値: 15）
    const CF_HDROP: u16 = 15;

    #[implement(IDataObject)]
    struct FileDataObject {
        files_data: Vec<u8>,
    }

    impl FileDataObject {
        fn new(paths: &[&str]) -> Self {
            let mut wide_chars: Vec<u16> = Vec::new();
            for path in paths {
                let wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
                wide_chars.extend(wide);
                wide_chars.push(0); // 各パスが null 終端
            }
            // DROPFILES のファイル名リスト末尾は二重 null 終端
            wide_chars.push(0);

            let dropfiles = DROPFILES {
                pFiles: std::mem::size_of::<DROPFILES>() as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: false.into(),
                fWide: true.into(), // Unicode (UTF-16)
            };

            let dropfiles_size = std::mem::size_of::<DROPFILES>();
            let wide_bytes_size = wide_chars.len() * std::mem::size_of::<u16>();
            let total_size = dropfiles_size + wide_bytes_size;

            let mut files_data = vec![0u8; total_size];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &dropfiles as *const _ as *const u8,
                    files_data.as_mut_ptr(),
                    dropfiles_size,
                );
                std::ptr::copy_nonoverlapping(
                    wide_chars.as_ptr() as *const u8,
                    files_data.as_mut_ptr().add(dropfiles_size),
                    wide_bytes_size,
                );
            }

            Self { files_data }
        }
    }

    #[allow(non_snake_case)]
    impl IDataObject_Impl for FileDataObject_Impl {
        fn GetData(&self, pformatetcinst: *const FORMATETC) -> Result<STGMEDIUM> {
            unsafe {
                if pformatetcinst.is_null() {
                    return Err(DV_E_FORMATETC.into());
                }
                let formatetc = &*pformatetcinst;
                if formatetc.cfFormat != CF_HDROP || (formatetc.tymed & TYMED_HGLOBAL.0 as u32) == 0 {
                    return Err(DV_E_FORMATETC.into());
                }

                let hglobal = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, self.files_data.len())?;
                let ptr = GlobalLock(hglobal);
                if ptr.is_null() {
                    return Err(windows::core::Error::from_win32());
                }
                std::ptr::copy_nonoverlapping(self.files_data.as_ptr(), ptr as *mut u8, self.files_data.len());
                let _ = GlobalUnlock(hglobal);

                let mut medium = STGMEDIUM::default();
                medium.tymed = TYMED_HGLOBAL.0 as u32;
                medium.u.hGlobal = hglobal;
                medium.pUnkForRelease = core::mem::ManuallyDrop::new(None);
                Ok(medium)
            }
        }

        fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            unsafe {
                if pformatetc.is_null() {
                    return DV_E_FORMATETC;
                }
                let format = &*pformatetc;
                if format.cfFormat == CF_HDROP && (format.tymed & TYMED_HGLOBAL.0 as u32) != 0 {
                    S_OK
                } else {
                    DV_E_FORMATETC
                }
            }
        }

        fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, _pformatetcout: *mut FORMATETC) -> HRESULT {
            E_NOTIMPL
        }

        fn SetData(&self, _pformatetc: *const FORMATETC, _pmedium: *const STGMEDIUM, _frelease: windows::Win32::Foundation::BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, _dwdirection: u32) -> Result<IEnumFORMATETC> {
            Err(E_NOTIMPL.into())
        }

        fn DAdvise(&self, _pformatetc: *const FORMATETC, _advf: u32, _padvsink: Option<&IAdviseSink>) -> Result<u32> {
            Err(E_NOTIMPL.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(E_NOTIMPL.into())
        }
    }

    #[implement(IDropSource)]
    struct FileDropSource;

    #[allow(non_snake_case)]
    impl IDropSource_Impl for FileDropSource_Impl {
        fn QueryContinueDrag(&self, fescapepressed: windows::Win32::Foundation::BOOL, _grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS) -> HRESULT {
            unsafe {
                if fescapepressed.as_bool() || (GetKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0 {
                    return DRAGDROP_S_CANCEL;
                }
                // マウス左ボタンが離されたらドロップ実行
                if (GetKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) == 0 {
                    return DRAGDROP_S_DROP;
                }
                S_OK
            }
        }

        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    /// 複数ファイル/ディレクトリパスで OLE ドラッグ＆ドロップを開始する。
    pub fn start_drag(paths: &[&str]) {
        if paths.is_empty() {
            return;
        }
        ensure_ole_init();

        let data_object: IDataObject = FileDataObject::new(paths).into();
        let drop_source: IDropSource = FileDropSource.into();

        let mut final_effect = DROPEFFECT::default();
        unsafe {
            let _ = DoDragDrop(
                &data_object,
                &drop_source,
                DROPEFFECT_COPY,
                &mut final_effect,
            );
        }
    }

    /// 単一ファイル/ディレクトリパスで OLE ドラッグ＆ドロップを開始する。
    pub fn start_drag_single(path: &str) {
        start_drag(&[path]);
    }
}

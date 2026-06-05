use std::{marker::PhantomData, rc::Rc, thread::Builder};
use windows::Win32::System::Com::{self, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};

use crate::{Error, Result};

#[derive(Debug)]
pub(super) struct ComApartment {
    _thread_bound: PhantomData<Rc<()>>,
}

impl ComApartment {
    fn new() -> Result<Self> {
        // SAFETY: `CoInitializeEx` initializes COM for the current thread with valid
        // flags. `ComApartment` calls `CoUninitialize` after a successful initialization.
        let result =
            unsafe { Com::CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };

        result.ok().map_err(|source| Error::Platform {
            message: format!("Failed to initialize Windows COM apartment: {source}"),
        })?;

        Ok(Self {
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: `ComApartment` is constructed after `CoInitializeEx` succeeds
        // and is dropped on the same worker thread that initialized COM.
        unsafe {
            Com::CoUninitialize();
        }
    }
}

pub(super) fn run_on_sta_thread<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&ComApartment) -> Result<T> + Send + 'static,
{
    let thread = Builder::new()
        .name("junkyard-discard".to_owned())
        .spawn(move || {
            let com_apartment = ComApartment::new()?;

            operation(&com_apartment)
        })
        .map_err(|source| Error::Platform {
            message: format!("Failed to spawn Windows trash thread: {source}"),
        })?;

    match thread.join() {
        Ok(result) => result,
        Err(_) => Err(Error::Platform {
            message: "Windows trash thread panicked".to_owned(),
        }),
    }
}

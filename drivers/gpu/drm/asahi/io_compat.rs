// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! The 7.1-style accessors the J813 GPU code uses on an [`IoMem`], over the
//! 7.3 views; the 7.3 `Io` trait already reads and writes an `&IoMem`.

use kernel::{
    io::{mem::IoMem, IoBase, Mmio, Region, RelaxedMmio},
};

pub(crate) trait IoMemCompat<'a, const SIZE: usize> {
    fn relaxed(self) -> RelaxedMmio<'a, Region<SIZE>>;
    fn view(self) -> Mmio<'a, Region<SIZE>>;
}

impl<'a, const SIZE: usize> IoMemCompat<'a, SIZE> for &'a IoMem<'_, SIZE> {
    fn relaxed(self) -> RelaxedMmio<'a, Region<SIZE>> {
        self.as_view().relaxed()
    }
    fn view(self) -> Mmio<'a, Region<SIZE>> {
        self.as_view()
    }
}

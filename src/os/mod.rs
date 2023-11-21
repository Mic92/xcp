/*
 * Copyright © 2018, Steve Smith <tarkasteve@gmail.com>
 *
 * This program is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License version
 * 3 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

mod common;

use cfg_if::cfg_if;
cfg_if! {
    if #[cfg(all(target_os = "linux", feature = "force_userspace"))] {
        mod linux;
        pub use linux::{
            probably_sparse,
            next_sparse_segments,
            map_extents,
        };
        pub use common::{
            allocate_file,
            copy_file_bytes,
            copy_file_offset,
            copy_permissions,
            merge_extents,
            is_same_file,
        };

    } else if #[cfg(target_os = "linux")] {
        mod linux;
        pub use linux::{
            copy_file_bytes,
            copy_file_offset,
            probably_sparse,
            next_sparse_segments,
            map_extents,
        };
        pub use common::{
            allocate_file,
            copy_permissions,
            merge_extents,
            is_same_file,
        };

    } else {
        pub use common::{
            allocate_file,
            copy_file_bytes,
            copy_file_offset,
            copy_permissions,
            probably_sparse,
            next_sparse_segments,
            merge_extents,
            map_extents,
            is_same_file,
        };
    }
}

// NOTE: The xattr crate has a SUPPORTED_PLATFORM flag, however it
// allows NetBSD, which fails for us, so we stick to platforms we've
// tested.
pub const XATTR_SUPPORTED: bool = {
    cfg_if! {
        if #[cfg(any(target_os = "linux", target_os = "freebsd"))] {
            true
        } else {
            false
        }
    }
};

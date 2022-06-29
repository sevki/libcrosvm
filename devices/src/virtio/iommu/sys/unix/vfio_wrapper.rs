// Copyright 2022 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Wraps VfioContainer for virtio-iommu implementation

use std::sync::Arc;

use anyhow::{bail, Context};
use base::{AsRawDescriptor, AsRawDescriptors, Protection, RawDescriptor};
use sync::Mutex;
use vm_memory::{GuestAddress, GuestMemory};

use crate::vfio::VfioError;
use crate::virtio::iommu::memory_mapper::{
    AddMapResult, MappingInfo, MemRegion, MemoryMapper, Translate,
};
use crate::VfioContainer;

pub struct VfioWrapper {
    container: Arc<Mutex<VfioContainer>>,
    // ID of the VFIO group which constitutes the container. Note that we rely on
    // the fact that no container contains multiple groups.
    id: u32,
    mem: GuestMemory,
}

impl VfioWrapper {
    pub fn new(container: Arc<Mutex<VfioContainer>>, mem: GuestMemory) -> Self {
        let c = container.lock();
        let groups = c.group_ids();
        // NOTE: vfio_get_container ensures each group gets its own container.
        assert!(groups.len() == 1);
        let id = *groups[0];
        drop(c);
        Self { container, id, mem }
    }

    pub fn new_with_id(container: VfioContainer, id: u32, mem: GuestMemory) -> Self {
        Self {
            container: Arc::new(Mutex::new(container)),
            id,
            mem,
        }
    }

    pub fn clone_as_raw_descriptor(&self) -> Result<RawDescriptor, VfioError> {
        self.container.lock().clone_as_raw_descriptor()
    }
}

impl MemoryMapper for VfioWrapper {
    fn add_map(&mut self, mut map: MappingInfo) -> anyhow::Result<AddMapResult> {
        map.gpa = GuestAddress(
            self.mem
                .get_host_address_range(map.gpa, map.size as usize)
                .context("failed to find host address")? as u64,
        );
        // Safe because both guest and host address are guaranteed by
        // get_host_address_range() to be valid.
        let res = unsafe {
            self.container.lock().vfio_dma_map(
                map.iova,
                map.size,
                map.gpa.offset(),
                map.prot.allows(&Protection::write()),
            )
        };
        if let Err(VfioError::IommuDmaMap(err)) = res {
            if err.errno() == libc::EEXIST {
                // A mapping already exists in the requested range,
                return Ok(AddMapResult::OverlapFailure);
            }
        }
        res.context("vfio mapping error").map(|_| AddMapResult::Ok)
    }

    fn remove_map(&mut self, iova_start: u64, size: u64) -> anyhow::Result<bool> {
        iova_start.checked_add(size).context("iova overflow")?;
        self.container
            .lock()
            .vfio_dma_unmap(iova_start, size)
            .context("vfio unmapping error")
            .map(|_| true)
    }

    fn get_mask(&self) -> anyhow::Result<u64> {
        self.container
            .lock()
            .vfio_get_iommu_page_size_mask()
            .context("vfio get mask error")
    }

    fn supports_detach(&self) -> bool {
        // A few reasons why we don't support detach:
        //
        // 1. Seems it's not possible to dynamically attach and detach a IOMMU domain if the
        //    virtio IOMMU device is running on top of VFIO
        // 2. Even if VIRTIO_IOMMU_T_DETACH is implemented in front-end driver, it could violate
        //    the following virtio IOMMU spec: Detach an endpoint from a domain. when this request
        //    completes, the endpoint cannot access any mapping from that domain anymore.
        //
        //    This is because VFIO doesn't support detaching a single device. When the virtio-iommu
        //    device receives a VIRTIO_IOMMU_T_DETACH request, it can either to:
        //    - detach a group: any other endpoints in the group lose access to the domain.
        //    - do not detach the group at all: this breaks the above mentioned spec.
        false
    }

    fn id(&self) -> u32 {
        self.id
    }
}

impl Translate for VfioWrapper {
    fn translate(&self, _iova: u64, _size: u64) -> anyhow::Result<Vec<MemRegion>> {
        bail!("unimplemented");
    }
}

impl AsRawDescriptors for VfioWrapper {
    fn as_raw_descriptors(&self) -> Vec<RawDescriptor> {
        vec![self.container.lock().as_raw_descriptor()]
    }
}

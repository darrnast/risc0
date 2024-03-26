// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::rc::Rc;

use cust::prelude::*;
use risc0_core::field::{
    baby_bear::{BabyBearElem, BabyBearExtElem},
    map_pow, Elem, ExtElem, RootsOfUnity,
};
use risc0_zkp::{
    core::log2_ceil,
    hal::{
        cuda::{
            BufferImpl as CudaBuffer, CudaHal, CudaHalSha256, CudaHash, CudaHashPoseidon2,
            CudaHashSha256,
        },
        Buffer, CircuitHal, Hal,
    },
    INV_RATE,
};

use crate::{
    prove::{engine::SegmentProverImpl, SegmentProver},
    GLOBAL_MIX, GLOBAL_OUT, REGISTER_GROUP_ACCUM, REGISTER_GROUP_CTRL, REGISTER_GROUP_DATA,
};

const KERNELS_FATBIN: &[u8] = include_bytes!(env!("RV32IM_CUDA_PATH"));

pub struct CudaCircuitHal<CH: CudaHash> {
    hal: Rc<CudaHal<CH>>, // retain a reference to ensure the context remains valid
    module: Module,
}

impl<CH: CudaHash> CudaCircuitHal<CH> {
    #[tracing::instrument(name = "CudaCircuitHal::new", skip_all)]
    pub fn new(hal: Rc<CudaHal<CH>>) -> Self {
        let module = Module::from_fatbin(KERNELS_FATBIN, &[]).unwrap();
        Self { hal, module }
    }
}

impl<'a, CH: CudaHash> CircuitHal<CudaHal<CH>> for CudaCircuitHal<CH> {
    #[tracing::instrument(skip_all)]
    fn eval_check(
        &self,
        check: &CudaBuffer<BabyBearElem>,
        groups: &[&CudaBuffer<BabyBearElem>],
        globals: &[&CudaBuffer<BabyBearElem>],
        poly_mix: BabyBearExtElem,
        po2: usize,
        steps: usize,
    ) {
        let ctrl = groups[REGISTER_GROUP_CTRL];
        let data = groups[REGISTER_GROUP_DATA];
        let accum = groups[REGISTER_GROUP_ACCUM];
        let mix = globals[GLOBAL_MIX];
        let out = globals[GLOBAL_OUT];
        tracing::debug!(
            "check: {}, ctrl: {}, data: {}, accum: {}, mix: {} out: {}",
            check.size(),
            ctrl.size(),
            data.size(),
            accum.size(),
            mix.size(),
            out.size()
        );
        tracing::debug!(
            "total: {}",
            (check.size() + ctrl.size() + data.size() + accum.size() + mix.size() + out.size()) * 4
        );

        const EXP_PO2: usize = log2_ceil(INV_RATE);
        let domain = steps * INV_RATE;
        let rou = BabyBearElem::ROU_FWD[po2 + EXP_PO2];

        let rou = self.hal.copy_from_elem("rou", &[rou]);
        let po2 = self.hal.copy_from_u32("po2", &[po2 as u32]);
        let size = self.hal.copy_from_u32("size", &[domain as u32]);

        let poly_mix_pows = map_pow(poly_mix, crate::info::POLY_MIX_POWERS);
        let poly_mix_pows: &[u32; BabyBearExtElem::EXT_SIZE * crate::info::NUM_POLY_MIX_POWERS] =
            BabyBearExtElem::as_u32_slice(poly_mix_pows.as_slice())
                .try_into()
                .unwrap();

        let mix_pows_name = std::ffi::CString::new("poly_mix").unwrap();
        self.module
            .get_global(&mix_pows_name)
            .unwrap()
            .copy_from(poly_mix_pows)
            .unwrap();

        let stream = Stream::new(StreamFlags::DEFAULT, None).unwrap();

        let kernel = self.module.get_function("eval_check").unwrap();
        let params = self.hal.compute_simple_params(domain);
        unsafe {
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                check.as_device_ptr(),
                ctrl.as_device_ptr(),
                data.as_device_ptr(),
                accum.as_device_ptr(),
                mix.as_device_ptr(),
                out.as_device_ptr(),
                rou.as_device_ptr(),
                po2.as_device_ptr(),
                size.as_device_ptr()
            ))
            .unwrap();
        }
        stream.synchronize().unwrap();
    }
}

pub type CudaCircuitHalSha256 = CudaCircuitHal<CudaHashSha256>;
pub type CudaCircuitHalPoseidon2 = CudaCircuitHal<CudaHashPoseidon2>;

pub fn get_segment_prover() -> Box<dyn SegmentProver> {
    let hal = Rc::new(CudaHalSha256::new());
    let circuit_hal = Rc::new(CudaCircuitHalSha256::new(hal.clone()));
    Box::new(SegmentProverImpl::new(hal, circuit_hal))
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use risc0_core::field::baby_bear::BabyBear;
    use risc0_zkp::{
        core::hash::sha::Sha256HashSuite,
        hal::{cpu::CpuHal, cuda::CudaHalSha256},
    };
    use test_log::test;

    use crate::prove::hal::cpu::CpuCircuitHal;

    #[test]
    fn eval_check() {
        const PO2: usize = 4;
        let cpu_hal: CpuHal<BabyBear> = CpuHal::new(Sha256HashSuite::new_suite());
        let cpu_eval = CpuCircuitHal::new();
        let gpu_hal = Rc::new(CudaHalSha256::new());
        let gpu_eval = super::CudaCircuitHal::new(gpu_hal.clone());
        crate::prove::hal::testutil::eval_check(
            &cpu_hal,
            cpu_eval,
            gpu_hal.as_ref(),
            gpu_eval,
            PO2,
        );
    }
}
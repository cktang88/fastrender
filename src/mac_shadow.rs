//! v3 shadow maps of `ThreeScene` (src/mac_three.rs, whose child module this is): the maps kept across frames,
//! per-frame preparation (casters, FrFrame / FrObject per pass, VSM blur jobs) and their encoding.

use super::*;

/// Bytes reserved per VSM blur program's uniform block (resolution, radius: 16 bytes in practice).
pub(super) const VSM_BLOCK: usize = 512;

/// One rendered shadow map (a SHADOWS entry), kept across frames while its kind / index / size stay the same.
pub(super) struct ShadowMap {
    kind: u32,
    index: u32,
    size: (usize, usize),
    pub(super) cube: bool,
    /// Depth32Float: what PCF (compare sampler) and Basic (nearest) read; VSM's blur input
    pub(super) depth: Texture,
    /// VSM: (map: RG16F mean / std-dev that three's programs sample, mapPass: the blur's intermediate)
    pub(super) vsm: Option<(Texture, Texture)>,
    /// older three (r170): the RGBA8 map three's programs sample (unpackRGBAToDepth); `depth` is its depth buffer
    pub(super) packed: Option<Texture>,
    /// rendered at least once (a kept map that never was binds the lit map instead)
    rendered: bool,
}

impl ShadowMap {
    fn new(dev: &Device, kind: u32, index: u32, size: (usize, usize), cube: bool, vsm: bool, packed: bool) -> ShadowMap {
        let make = |fmt: MTLPixelFormat, ty: MTLTextureType| -> Texture {
            let d = MTLTextureDescriptor::new();
            d.setTextureType(ty);
            d.setPixelFormat(fmt);
            unsafe {
                d.setWidth(size.0);
                d.setHeight(size.1);
            }
            d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
            d.setStorageMode(MTLStorageMode::Private);
            dev.newTextureWithDescriptor(&d).expect("shadow map")
        };
        let depth = make(MTLPixelFormat::Depth32Float, if cube { MTLTextureType::TypeCube } else { MTLTextureType::Type2D });
        let vsm = vsm.then(|| (make(MTLPixelFormat::RG16Float, MTLTextureType::Type2D), make(MTLPixelFormat::RG16Float, MTLTextureType::Type2D)));
        let packed = packed.then(|| make(MTLPixelFormat::RGBA8Unorm, MTLTextureType::Type2D));
        ShadowMap { kind, index, size, cube, depth, vsm, packed, rendered: false }
    }
}

/// A caster drawn with three's own depth / distance program.
#[derive(Clone, Copy)]
pub(super) struct ShadowProg {
    block: usize,
    tex: usize,
    pipe: u32,
    depth: u32,
    /// the depth material instance id
    mat: u32,
    side: u32,
    wireframe: bool,
}

/// One caster in one shadow pass.
#[derive(Clone, Copy)]
pub(super) struct ShadowDraw3 {
    /// index into `draws`
    i: usize,
    /// its FrObject (identity view) in the ring
    obj: usize,
    /// None: the native depth-only path (`vs_three_depth`)
    pub(super) prog: Option<ShadowProg>,
    /// side rendered: 0 front, 1 back, 2 both
    side: u32,
    wireframe: bool,
    /// depth state of the native path
    depth: u32,
}

/// One depth pass: a 2D map, or one face of a cube map.
pub(super) struct ShadowJob {
    map: usize,
    face: usize,
    /// ring offsets of this pass's FrFrame and FrPass
    frame: usize,
    pass: usize,
    draws: std::ops::Range<usize>,
}

/// three's VSMPass for one map: two of three's own programs (vertical, horizontal blur).
pub(super) struct VsmJob {
    map: usize,
    pass: usize,
    blocks: [usize; 2],
    pipes: [u32; 2],
    progs: [u32; 2],
}

/// World-space bounding sphere of a draw (geometry sphere through its 3x4 rows; scale = the largest axis).
fn world_sphere(r: &[[f32; 4]; 3], s: [f32; 4]) -> (Vec3, f32) {
    let c = Vec3::new(
        r[0][0] * s[0] + r[0][1] * s[1] + r[0][2] * s[2] + r[0][3],
        r[1][0] * s[0] + r[1][1] * s[1] + r[1][2] * s[2] + r[1][3],
        r[2][0] * s[0] + r[2][1] * s[1] + r[2][2] * s[2] + r[2][3],
    );
    let scale = (0..3).map(|k| Vec3::new(r[0][k], r[1][k], r[2][k]).length()).fold(0.0, f32::max);
    (c, s[3] * scale)
}

/// The view-projection (OpenGL clip space) three renders cube face `face` of a point light's shadow with
/// (WebGLShadowMap: PerspectiveCamera(90, 1, near, far) at the light, looking down _cubeDirections[face] with
/// _cubeUps[face]).
fn point_face_vp(pos: Vec3, near: f32, far: f32, face: usize) -> Mat4 {
    const DIRS: [[f32; 3]; 6] = [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
    const UPS: [[f32; 3]; 6] = [[0.0, -1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0], [0.0, -1.0, 0.0], [0.0, -1.0, 0.0]];
    let proj = glam::camera::rh::proj::opengl::perspective(std::f32::consts::FRAC_PI_2, 1.0, near, far);
    let view = glam::camera::rh::view::look_to_mat4(pos, Vec3::from(DIRS[face]), Vec3::from(UPS[face]));
    proj * view
}

#[cfg(test)]
mod shadow_tests {
    use super::*;

    #[test]
    fn point_faces_cover_their_axis() {
        let pos = Vec3::new(1.0, 2.0, 3.0);
        for (face, dir) in [Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z].into_iter().enumerate() {
            let vp = point_face_vp(pos, 0.5, 30.0, face);
            let c = vp * (pos + dir * 5.0).extend(1.0);
            assert!((c.x / c.w).abs() < 1e-5 && (c.y / c.w).abs() < 1e-5, "face {face}: the axis is the centre");
            let z = c.z / c.w;
            assert!(z > -1.0 && z < 1.0, "face {face}: inside the depth range");
        }
        // +X face: up is -Y, so a point above the axis lands at negative clip y (three's _cubeUps)
        let vp = point_face_vp(Vec3::ZERO, 0.5, 30.0, 0);
        let c = vp * Vec4::new(5.0, 1.0, 0.0, 1.0);
        assert!(c.y / c.w < 0.0);
    }
}

impl ThreeScene {
    /// v3 shadow maps for this frame: one per SHADOWS entry three renders (r180+ depth textures: directional and spot
    /// lights a 2D map, point lights a cube; VSM: plus an RG16F map blurred by three's VSM programs), each caster drawn
    /// with three's depth program (alphaTest, skinning, morphs, custom depth materials) or the native depth-only path.
    /// FrFrame per pass: projectionMatrix = the shadow camera's view-projection, y-flipped so the rows are stored as WebGL
    /// stores them (three's shadow matrices then sample them unchanged); FrObject per caster with an identity view.
    pub(super) fn prepare_shadows_v3(&mut self, dev: &Device, slot_i: usize) {
        self.shadow_jobs.clear();
        self.shadow_list.clear();
        self.vsm_jobs.clear();
        self.shadow_lookup.clear();
        // older three (r170) packs depth into RGBA8 maps (MeshDepthMaterial, RGBADepthPacking): rendered natively for
        // directional / spot lights (the CSM cascades); its point maps (4:2 packed distance) and VSM bind a lit map
        let packed = !self.shadow_depth_textures;
        if packed && self.shadow_ty == three::shadow_type::VSM || self.dbg & 2 != 0 {
            return; // every shadow slot samples a lit map
        }
        let rw = self.ring_writer(slot_i);
        let vsm = self.shadow_ty == three::shadow_type::VSM;
        // every FrFrame / FrPass / VSM block of the shadow passes comes from the shadow area (RingLayout sized it from
        // the same SHADOWS list); a full area ends the shadow passes for this frame instead of writing past it
        let mut area = Bump::new(self.layout.shadow_area, self.layout.shadow_area_end);
        // native depth-only caster: three's MeshDepthMaterial state (LessEqual, depth write)
        let depth_le = self.depth_le;
        let flip_y = Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0));
        let mut written: Vec<bool> = vec![false; self.draws.len()];
        let shadows = self.shadows.clone();
        for (k, rec) in shadows.iter().enumerate() {
            let point = rec.kind == three::tex::POINT;
            if point && (vsm || packed) {
                continue; // three: "VSM shadow maps are not supported for PointLights"; r170 point maps: not rendered
            }
            let (w, h) = ((rec.map_size[0].max(1.0) as usize).min(8192), (rec.map_size[1].max(1.0) as usize).min(8192));
            let (w, h) = if point { (w, w) } else { (w, h) };
            let want_vsm = vsm && self.shadow_vsm.get(k).is_some_and(|p| p[0] != 0 && p[1] != 0);
            let found = self.shadow_maps.iter().position(|m| m.kind == rec.kind && m.index == rec.index);
            if self.shadow_keep.get(k).copied().unwrap_or(false) {
                // three skips this light's shadow pass: its last map (and the matching shadow matrix three kept) stand
                if let Some(m) = found.filter(|&m| self.shadow_maps[m].rendered) {
                    self.shadow_lookup.push((rec.kind, rec.index, m));
                }
                continue;
            }
            let map = match found {
                Some(m) if self.shadow_maps[m].size == (w, h) && self.shadow_maps[m].vsm.is_some() == want_vsm && self.shadow_maps[m].packed.is_some() == packed => m,
                found => {
                    let m = ShadowMap::new(dev, rec.kind, rec.index, (w, h), point, want_vsm, packed);
                    match found {
                        Some(i) => {
                            self.shadow_maps[i] = m;
                            i
                        }
                        None => {
                            self.shadow_maps.push(m);
                            self.shadow_maps.len() - 1
                        }
                    }
                }
            };
            self.shadow_maps[map].rendered = true;
            self.shadow_lookup.push((rec.kind, rec.index, map));
            let pos = Vec3::from(rec.pos);
            for face in 0..if point { 6 } else { 1 } {
                let vp = if point { point_face_vp(pos, rec.near, rec.far, face) } else { Mat4::from_cols_array(&rec.vp) };
                let planes = frustum_planes(&three::gl_to_metal(&vp));
                let (Some(frame), Some(pass)) = (area.take(RING_ALIGN), area.take(RING_ALIGN)) else { return };
                let fr = FrFrame { projection: (flip_y * vp).to_cols_array(), view: Mat4::IDENTITY.to_cols_array(), camera_position: [pos.x, pos.y, pos.z, 0.0] };
                rw.put(frame, fr);
                rw.put(pass, [h as f32, 0.0, 0.0, 0.0]);
                let first = self.shadow_list.len();
                for i in 0..self.draws.len() {
                    let d = self.draws[i];
                    if d.flags & flag::CAST == 0 && !(vsm && d.flags & flag::RECEIVE != 0) || d.count == 0 {
                        continue;
                    }
                    let Some(g) = self.geos.get(&d.geometry) else { continue };
                    let Some(main) = self.mats3.get(&d.material) else { continue };
                    if d.flags & (flag::NO_CULL | flag::INSTANCED) == 0 {
                        let (c, r) = world_sphere(&d.rows, g.sphere);
                        if !sphere_visible(&planes, c, r) {
                            continue;
                        }
                    }
                    let main_meta = main.meta.clone();
                    // packed maps: every caster on the native path (three's packing depth programs are not sent)
                    let id = if packed { 0 } else { self.shadow_ids.get(i).map_or(0, |p| if point { p[1] } else { p[0] }) };
                    let prog = if id != 0 {
                        match (self.uni.get(&id).copied(), self.instance_pipeline(dev, id, &d, Target::Depth)) {
                            (Some((block, tex)), Some(pipe)) => {
                                let m = &self.mats3[&id].meta;
                                Some(ShadowProg { block, tex, pipe, depth: self.mats3[&id].depth, mat: id, side: m.side, wireframe: m.wireframe })
                            }
                            _ => None, // the depth program failed (reported): native depth-only
                        }
                    } else {
                        None
                    };
                    let obj = self.layout.shadow_objects + i * align(std::mem::size_of::<FrObject>());
                    if !written[i] {
                        if !rw.put(obj, fr_object(&Mat4::IDENTITY, &d.rows)) {
                            continue;
                        }
                        written[i] = true;
                    }
                    // side rendered (WebGLShadowMap.getDepthMaterial): shadowSide, else the opposite side (VSM: the same)
                    let side = match &prog {
                        Some(p) => p.side,
                        None => main_meta.shadow_side.unwrap_or(if vsm { main_meta.side } else { match main_meta.side { 0 => 1, 1 => 0, s => s } }),
                    };
                    self.shadow_list.push(ShadowDraw3 { i, obj, prog, side, wireframe: prog.map_or(main_meta.wireframe, |p| p.wireframe), depth: depth_le });
                }
                self.shadow_jobs.push(ShadowJob { map, face, frame, pass, draws: first..self.shadow_list.len() });
            }
            if want_vsm {
                // three's VSMPass: vertical blur of the depth map into mapPass, horizontal back into map (RG16F)
                let ids = self.shadow_vsm[k];
                let Some(pass) = area.take(RING_ALIGN) else { return };
                rw.put(pass, [h as f32, 0.0, 0.0, 0.0]);
                let mut job = VsmJob { map, pass, blocks: [0; 2], pipes: [u32::MAX; 2], progs: ids };
                for (n, &pid) in ids.iter().enumerate() {
                    let Some(block) = area.take(VSM_BLOCK) else { return };
                    job.blocks[n] = block;
                    let Some(p) = self.progs.progs.get(&pid) else { continue };
                    if p.layout.size as usize > VSM_BLOCK {
                        continue;
                    }
                    rw.zero(block, VSM_BLOCK);
                    for m in &p.layout.members {
                        let at = block + m.offset as usize;
                        match m.name.as_str() {
                            "resolution" => {
                                rw.put(at, [w as f32, h as f32]);
                            }
                            "radius" => {
                                rw.put(at, rec.radius);
                            }
                            _ => {}
                        }
                    }
                    let pos_name = self.progs.name_id_get("position");
                    let sig = p.attrs.iter().enumerate().fold(0, |s, (a_i, a)| s | if Some(a.name) == pos_name { 3u64 << (4 * a_i) } else { 0 });
                    let meta: ProgramMaterialMeta = serde_json::from_str(&format!("{{\"program\":{pid},\"name\":\"VSM\",\"type\":\"ShaderMaterial\"}}")).expect("meta");
                    job.pipes[n] = self.progs.pipeline(dev, &meta, sig, Target::Rg16).unwrap_or(u32::MAX);
                }
                self.vsm_jobs.push(job);
            }
        }
    }

    /// v3: render every shadow map prepared for `slot_i` (before the main pass, in the same command buffer).
    pub fn encode_shadows_v3(&self, cb: &ProtocolObject<dyn MTLCommandBuffer>, slot_i: usize) {
        if !self.view.v3 {
            return;
        }
        let ring = &self.slots[slot_i].ring;
        for job in &self.shadow_jobs {
            let map = &self.shadow_maps[job.map];
            let rp = MTLRenderPassDescriptor::new();
            let da = rp.depthAttachment();
            da.setTexture(Some(&map.depth));
            if map.cube {
                da.setSlice(job.face);
            }
            da.setLoadAction(MTLLoadAction::Clear);
            da.setStoreAction(if map.packed.is_some() { MTLStoreAction::DontCare } else { MTLStoreAction::Store });
            da.setClearDepth(1.0);
            if let Some(c) = &map.packed {
                // WebGLShadowMap (r170) clears to white: depth 1, nothing in shadow
                let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(c));
                a.setLoadAction(MTLLoadAction::Clear);
                a.setStoreAction(MTLStoreAction::Store);
                a.setClearColor(MTLClearColor { red: 1.0, green: 1.0, blue: 1.0, alpha: 1.0 });
            }
            let Some(enc) = cb.renderCommandEncoderWithDescriptor(&rp) else { continue };
            enc.setViewport(MTLViewport { originX: 0.0, originY: 0.0, width: map.size.0 as f64, height: map.size.1 as f64, znear: 0.0, zfar: 1.0 });
            unsafe {
                for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER] {
                    enc.setVertexBuffer_offset_atIndex(Some(ring), 0, b as usize);
                    enc.setFragmentBuffer_offset_atIndex(Some(ring), 0, b as usize);
                }
                enc.setVertexBuffer_offset_atIndex(Some(ring), job.frame, xlat::FRAME_BLOCK_BUFFER as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), job.frame, xlat::FRAME_BLOCK_BUFFER as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), job.pass, xlat::PASS_BUFFER as usize);
            }
            let mut last_depth = u32::MAX;
            for sd in &self.shadow_list[job.draws.clone()] {
                let d = &self.draws[sd.i];
                let g = &self.geos[&d.geometry];
                // side rendered: 0 front (cull back), 1 back (cull front), 2 both; the y-flipped projection mirrors winding
                enc.setCullMode(match sd.side { 0 => MTLCullMode::Back, 1 => MTLCullMode::Front, _ => MTLCullMode::None });
                let mirrored = Self::winding(d) == MTLWinding::Clockwise;
                enc.setFrontFacingWinding(if mirrored { MTLWinding::CounterClockwise } else { MTLWinding::Clockwise });
                enc.setTriangleFillMode(if sd.wireframe { MTLTriangleFillMode::Lines } else { MTLTriangleFillMode::Fill });
                let depth = sd.prog.map_or(sd.depth, |p| p.depth);
                if depth != last_depth {
                    enc.setDepthStencilState(Some(self.progs.depth_at(depth)));
                    last_depth = depth;
                }
                unsafe { enc.setVertexBufferOffset_atIndex(sd.obj, xlat::OBJECT_BLOCK_BUFFER as usize) };
                match sd.prog {
                    Some(p) => {
                        let Some(pipe) = self.progs.pipeline_at(p.pipe) else { continue };
                        let Some(prog) = self.mats3.get(&p.mat).and_then(|m| self.progs.progs.get(&m.meta.program)) else { continue };
                        enc.setRenderPipelineState(pipe);
                        unsafe { enc.setFragmentBufferOffset_atIndex(sd.obj, xlat::OBJECT_BLOCK_BUFFER as usize) };
                        self.bind_program_draw(&enc, prog, d, g, p.block, p.tex);
                    }
                    None => {
                        enc.setRenderPipelineState(if map.packed.is_some() { &self.pipes.depth3_packed } else { &self.pipes.depth3 });
                        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
                        let instanced: u32 = inst.is_some() as u32;
                        unsafe {
                            enc.setVertexBuffer_offset_atIndex(Some(&g.pos), 0, 4);
                            enc.setVertexBuffer_offset_atIndex(Some(inst.map_or(&self.progs.defaults, |x| &x.cols)), 0, 5);
                            enc.setVertexBytes_length_atIndex(NonNull::new(&instanced as *const u32 as *mut c_void).unwrap(), 4, 6);
                        }
                    }
                }
                self.draw(&enc, d, g);
            }
            enc.setTriangleFillMode(MTLTriangleFillMode::Fill);
            enc.endEncoding();
        }
        for job in &self.vsm_jobs {
            let map = &self.shadow_maps[job.map];
            let Some((vmap, vpass)) = &map.vsm else { continue };
            let p = &self.progs;
            // vertical: depth (nearest) -> mapPass; horizontal: mapPass (linear) -> map
            for (n, (src, dst, smp)) in [(&map.depth, vpass, &p.nearest_clamp), (vpass, vmap, &p.linear_clamp)].into_iter().enumerate() {
                let (Some(pipe), Some(prog)) = (p.pipeline_at(job.pipes[n]), p.progs.get(&job.progs[n])) else { continue };
                let rp = MTLRenderPassDescriptor::new();
                let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(dst));
                a.setLoadAction(MTLLoadAction::DontCare);
                a.setStoreAction(MTLStoreAction::Store);
                let Some(enc) = cb.renderCommandEncoderWithDescriptor(&rp) else { continue };
                enc.setRenderPipelineState(pipe);
                enc.setCullMode(MTLCullMode::None);
                unsafe {
                    for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER, xlat::FRAME_BLOCK_BUFFER] {
                        let off = if b == xlat::DEFAULT_BLOCK_BUFFER { job.blocks[n] } else { self.layout.frame };
                        enc.setVertexBuffer_offset_atIndex(Some(ring), off, b as usize);
                        enc.setFragmentBuffer_offset_atIndex(Some(ring), off, b as usize);
                    }
                    enc.setFragmentBuffer_offset_atIndex(Some(ring), job.pass, xlat::PASS_BUFFER as usize);
                    for a in &prog.attrs {
                        enc.setVertexBuffer_offset_atIndex(Some(&self.fullscreen), 0, ATTR_BUFFER0 + a.location as usize);
                    }
                    for t in &prog.layout.textures {
                        if let Some(b) = t.fragment {
                            enc.setFragmentTexture_atIndex(Some(src), b.texture as usize);
                            if let Some(s) = b.sampler {
                                enc.setFragmentSamplerState_atIndex(Some(smp), s as usize);
                            }
                        }
                    }
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
                }
                enc.endEncoding();
            }
        }
    }
}

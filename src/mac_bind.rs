//! Per-draw binding of `ThreeScene` (src/mac_three.rs, whose child module this is): vertex stream signatures,
//! vertex streams / uniform block / textures of a program draw, texture resolution, and the draw call itself.

use super::*;

impl ThreeScene {
    /// How a program reads each of its attributes for draw `d` (see mac_programs::StreamSig).
    pub(super) fn stream_sig(&self, prog: &crate::mac_programs::Prog, d: &Draw, geo: &Geo) -> StreamSig {
        let inst_names = (self.progs.name_id_get("instanceMatrix"), self.progs.name_id_get("instanceColor"));
        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
        let mut sig: StreamSig = 0;
        for (a_i, a) in prog.attrs.iter().enumerate() {
            let s = if Some(a.name) == inst_names.0 {
                if inst.is_some() { SIG_INSTANCE | 4 } else { 0 }
            } else if Some(a.name) == inst_names.1 {
                if inst.is_some_and(|x| x.colors.is_some()) { SIG_INSTANCE | 3 } else { 0 }
            } else {
                match geo.attrs.iter().find(|g| g.0 == a.name) {
                    Some(g) => g.2 as u64,
                    None => geo.inst_attrs.iter().find(|g| g.0 == a.name).map_or(0, |g| SIG_INSTANCE | g.2 as u64),
                }
            };
            sig |= s << (4 * a_i);
        }
        sig
    }

    /// Vertex streams, uniform block, textures of a three program draw (main pass and shadow passes).
    pub(super) fn bind_program_draw(&self, enc: &RenderEnc, prog: &crate::mac_programs::Prog, d: &Draw, g: &Geo, block: usize, tex: usize) {
        let inst_names = (self.progs.name_id_get("instanceMatrix"), self.progs.name_id_get("instanceColor"));
        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
        unsafe {
            for a in &prog.attrs {
                let b = ATTR_BUFFER0 + a.location as usize;
                let (buf, off) = if Some(a.name) == inst_names.0 {
                    inst.map_or((&self.progs.defaults, 16), |x| (&x.cols, 0))
                } else if Some(a.name) == inst_names.1 {
                    inst.and_then(|x| x.colors.as_ref()).map_or((&self.progs.defaults, 0), |c| (c, 0))
                } else {
                    match g.attrs.iter().chain(g.inst_attrs.iter()).find(|x| x.0 == a.name) {
                        Some(x) => (&x.1, 0),
                        None => (&self.progs.defaults, if a.cols == 4 { 16 } else { 0 }),
                    }
                };
                enc.setVertexBuffer_offset_atIndex(Some(buf), off, b);
            }
            enc.setVertexBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
            enc.setFragmentBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
            if self.dbg & 64 != 0 {
                return;
            }
            for (k, t) in prog.layout.textures.iter().enumerate() {
                let id = self.uni_tex.get(tex + k).copied().unwrap_or(0);
                let (tx, smp) = self.resolve(id, t);
                if let Some(b) = t.fragment {
                    enc.setFragmentTexture_atIndex(Some(tx), b.texture as usize);
                    if let Some(s) = b.sampler.filter(|s| *s < MAX_SAMPLERS) {
                        enc.setFragmentSamplerState_atIndex(Some(smp), s as usize);
                    }
                }
                if let Some(b) = t.vertex {
                    enc.setVertexTexture_atIndex(Some(tx), b.texture as usize);
                    if let Some(s) = b.sampler.filter(|s| *s < MAX_SAMPLERS) {
                        enc.setVertexSamplerState_atIndex(Some(smp), s as usize);
                    }
                }
            }
        }
    }

    pub(super) fn bind_geometry(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(&g.pos), 0, 0);
            enc.setVertexBuffer_offset_atIndex(Some(&g.nrm), 0, 1);
            enc.setVertexBuffer_offset_atIndex(Some(g.uv.as_ref().unwrap_or(&self.dummy)), 0, 2);
            enc.setVertexBuffer_offset_atIndex(Some(g.col.as_ref().unwrap_or(&self.dummy)), 0, 4);
            let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
            enc.setVertexBuffer_offset_atIndex(Some(inst.map(|i| &i.rows).unwrap_or(&self.dummy)), 0, 6);
            enc.setVertexBuffer_offset_atIndex(Some(inst.and_then(|i| i.colors.as_ref()).unwrap_or(&self.dummy)), 0, 7);
        }
    }

    pub(super) fn draw(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        let n = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances).map(|_| d.instance_count as usize).unwrap_or(0) } else { 1 };
        // per-instance attribute streams hold this many instances: never step past them (a short stream would read
        // past its buffer)
        let n = g.inst_cap.map_or(n, |c| n.min(c));
        if n == 0 {
            return;
        }
        let prim = Self::primitive(d.flags);
        unsafe {
            if d.flags & flag::BATCH != 0 {
                // a BatchedMesh entry: three's gl_DrawID is the base instance (no per-instance streams are bound)
                let b = d.instances as usize;
                match &g.index {
                    Some((ib, ty)) => {
                        let stride = if *ty == MTLIndexType::UInt16 { 2 } else { 4 };
                        enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount_baseVertex_baseInstance(prim, d.count as usize, *ty, ib, d.start as usize * stride, 1, 0, b);
                    }
                    None => enc.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(prim, d.start as usize, d.count as usize, 1, b),
                }
                return;
            }
            match &g.index {
                Some((ib, ty)) => {
                    let stride = if *ty == MTLIndexType::UInt16 { 2 } else { 4 };
                    enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(prim, d.count as usize, *ty, ib, d.start as usize * stride, n);
                }
                None => enc.drawPrimitives_vertexStart_vertexCount_instanceCount(prim, d.start as usize, d.count as usize, n),
            }
        }
    }

    /// three's draw mode (WebGLRenderer.renderBufferDirect: points, lines, line strip / loop, else triangles)
    fn primitive(flags: u32) -> MTLPrimitiveType {
        if flags & flag::POINTS != 0 {
            MTLPrimitiveType::Point
        } else if flags & flag::LINES != 0 {
            MTLPrimitiveType::Line
        } else if flags & (flag::LINE_STRIP | flag::LINE_LOOP) != 0 {
            MTLPrimitiveType::LineStrip
        } else {
            MTLPrimitiveType::Triangle
        }
    }

    pub(super) fn winding(d: &Draw) -> MTLWinding {
        let r = &d.rows;
        let c0 = Vec3::new(r[0][0], r[1][0], r[2][0]);
        let c1 = Vec3::new(r[0][1], r[1][1], r[2][1]);
        let c2 = Vec3::new(r[0][2], r[1][2], r[2][2]);
        // a mirrored transform turns counter-clockwise faces clockwise (three.js does the same flip)
        if c0.dot(c1.cross(c2)) < 0.0 { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise }
    }

    /// The texture + sampler for texture slot `t` of a v3 program, given the id JS packed for it.
    fn resolve(&self, id: u32, t: &xlat::Texture) -> (&Texture, &Sampler) {
        let p = &self.progs;
        let n = &p.neutral;
        use three::tex;
        if id & 0xFF00_0000 == tex::SHADOW {
            let (kind, index) = ((id >> 16) & 0xFF, id & 0xFFFF);
            let vsm = self.shadow_ty == three::shadow_type::VSM;
            // PCF: compare sampler; Basic: nearest; VSM: the blurred RG16F map, linear (fixed states are constexpr
            // samplers in the MSL: these only matter for programs that kept a sampler array)
            let smp = if t.shadow { &p.compare } else if vsm { &p.linear_clamp } else { &p.nearest_clamp };
            let ours = self.shadow_lookup.iter().find(|m| m.0 == kind && m.1 == index).map(|m| &self.shadow_maps[m.2]).filter(|_| self.dbg & 1 == 0);
            if let Some(m) = ours.filter(|m| m.cube == (t.kind == "cube")) {
                if let Some(pk) = &m.packed {
                    return (pk, &p.nearest_clamp);
                }
                return (m.vsm.as_ref().map_or(&m.depth, |v| &v.0), smp);
            }
            let lit = if t.kind == "cube" { n.lit_cube.as_ref() } else { n.lit_2d.as_ref() };
            return match lit {
                Some(l) => (l, smp),
                None => (&n.white_2d, &p.linear_clamp),
            };
        }
        if t.sample != "float" {
            // integer samplers (three's batchingIdTexture): the uploaded integer texture of that kind, else a zero one
            let want = t.sample == "uint";
            if let Some(tx) = self.texs.get(&id).filter(|x| x.1.integer() == Some(want) && t.kind == "2d" && x.1.dim <= 1).map(|x| &x.0) {
                return (tx, &p.nearest_clamp);
            }
            return (if want { &n.uint_2d } else { &n.int_2d }, &p.nearest_clamp);
        }
        if self.dbg & 16 != 0 {
            return (&n.white_2d, &p.linear_clamp);
        }
        if id == tex::DFG_LUT && t.kind == "2d" {
            return (&p.dfg, &p.linear_clamp);
        }
        // a game texture, or a render target's attachment (mac_targets.rs)
        if let Some((tx, s)) = self.texs.get(&id).map(|(t, _, s)| (t, s)).or_else(|| self.targets.texture(id)) {
            let ty = tx.textureType();
            let fits = match t.kind.as_str() {
                "2d" => ty == MTLTextureType::Type2D,
                "2dArray" => ty == MTLTextureType::Type2DArray,
                "3d" => ty == MTLTextureType::Type3D,
                "cube" => ty == MTLTextureType::TypeCube,
                _ => false,
            };
            if fits {
                return (tx, s);
            }
        }
        let dummy = match t.kind.as_str() {
            "cube" => &n.white_cube,
            "2dArray" => &n.white_2d_array,
            "3d" => &n.white_3d,
            _ => &n.white_2d,
        };
        (dummy, &p.linear_clamp)
    }
}

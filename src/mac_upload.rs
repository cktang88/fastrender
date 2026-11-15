//! The resource registry of `ThreeScene` (src/mac_three.rs, whose child module this is): applying the frame's
//! resource messages (uploads / releases of geometry, textures, materials, instance buffers), and the samplers.
//!
//! Lifetime: an entry that is replaced (a re-upload under the same id: instance matrices every frame) or released
//! goes to the graveyard (`ThreeScene::bury`, src/gpu.rs), never straight to `drop`: frames in flight may still
//! read its buffers / textures and no backend is assumed to retain them. M-C: the same two places (`insert` and
//! `bury`) are where the Metal 4 residency set learns about a new allocation and forgets a dead one.

use super::*;

/// A CompressedTexture's mip chain (KTX2Loader's ASTC / BC / ETC output) as a Metal texture, blocks uploaded unchanged.
fn compressed_texture(dev: &Device, m: &TextureMeta, a: &[Option<Array>]) -> Result<Texture, String> {
    use three::objc2_metal_format::Fmt;
    let (f, bw, bh, bb) = three::compressed_format(m.format, m.srgb).ok_or_else(|| format!("compressed format {} not supported", m.format))?;
    let bc = !matches!(f, Fmt::Astc { .. } | Fmt::Etc2Rgb | Fmt::Etc2RgbSrgb | Fmt::Etc2Rgba | Fmt::Etc2RgbaSrgb | Fmt::EacR11 | Fmt::EacR11Snorm | Fmt::EacRg11 | Fmt::EacRg11Snorm);
    if bc && !dev.supportsBCTextureCompression() {
        return Err("BC texture compression is not supported by this GPU".into());
    }
    if !bc && !dev.supportsFamily(MTLGPUFamily::Apple2) {
        return Err("ASTC / ETC2 textures need an Apple GPU".into());
    }
    let fmt = match f {
        Fmt::Bc1 => MTLPixelFormat::BC1_RGBA,
        Fmt::Bc1Srgb => MTLPixelFormat::BC1_RGBA_sRGB,
        Fmt::Bc2 => MTLPixelFormat::BC2_RGBA,
        Fmt::Bc2Srgb => MTLPixelFormat::BC2_RGBA_sRGB,
        Fmt::Bc3 => MTLPixelFormat::BC3_RGBA,
        Fmt::Bc3Srgb => MTLPixelFormat::BC3_RGBA_sRGB,
        Fmt::Bc4 => MTLPixelFormat::BC4_RUnorm,
        Fmt::Bc4Snorm => MTLPixelFormat::BC4_RSnorm,
        Fmt::Bc5 => MTLPixelFormat::BC5_RGUnorm,
        Fmt::Bc5Snorm => MTLPixelFormat::BC5_RGSnorm,
        Fmt::Bc6hSigned => MTLPixelFormat::BC6H_RGBFloat,
        Fmt::Bc6hUnsigned => MTLPixelFormat::BC6H_RGBUfloat,
        Fmt::Bc7 => MTLPixelFormat::BC7_RGBAUnorm,
        Fmt::Bc7Srgb => MTLPixelFormat::BC7_RGBAUnorm_sRGB,
        Fmt::Etc2Rgb => MTLPixelFormat::ETC2_RGB8,
        Fmt::Etc2RgbSrgb => MTLPixelFormat::ETC2_RGB8_sRGB,
        Fmt::Etc2Rgba => MTLPixelFormat::EAC_RGBA8,
        Fmt::Etc2RgbaSrgb => MTLPixelFormat::EAC_RGBA8_sRGB,
        Fmt::EacR11 => MTLPixelFormat::EAC_R11Unorm,
        Fmt::EacR11Snorm => MTLPixelFormat::EAC_R11Snorm,
        Fmt::EacRg11 => MTLPixelFormat::EAC_RG11Unorm,
        Fmt::EacRg11Snorm => MTLPixelFormat::EAC_RG11Snorm,
        Fmt::Astc { w, h, srgb } => {
            // MTLPixelFormatASTC_*_sRGB = 186.. (no 189), *_LDR = sRGB + 18
            const SIZES: [(u32, u32); 14] = [(4, 4), (5, 4), (5, 5), (6, 5), (6, 6), (8, 5), (8, 6), (8, 8), (10, 5), (10, 6), (10, 8), (10, 10), (12, 10), (12, 12)];
            const SRGB: [usize; 14] = [186, 187, 188, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200];
            let k = SIZES.iter().position(|s| *s == (w, h)).ok_or("ASTC block size")?;
            MTLPixelFormat(SRGB[k] + if srgb { 0 } else { 18 })
        }
    };
    let levels = m.levels.max(1).min(a.len());
    let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, m.width, m.height, false) };
    unsafe { d.setMipmapLevelCount(levels) };
    d.setUsage(MTLTextureUsage::ShaderRead);
    let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
    for l in 0..levels {
        let (w, h) = ((m.width >> l).max(1), (m.height >> l).max(1));
        let (bx, by) = (w.div_ceil(bw as usize), h.div_ceil(bh as usize));
        let row = bx * bb as usize;
        let data = match &a[l] {
            Some(Array::U8(v)) if v.len() >= row * by => v,
            _ => return Err(format!("compressed level {l}: needs {} bytes", row * by)),
        };
        unsafe {
            t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w, height: h, depth: 1 } },
                l,
                NonNull::new(data.as_ptr() as *mut c_void).unwrap(),
                row,
            );
        }
    }
    Ok(t)
}

fn f32s(a: &Option<Array>) -> Option<&[f32]> { if let Some(Array::F32(v)) = a { Some(v) } else { None } }

/// A sampler matching three.js's filter/wrap constants (NearestFilter 1003 .. LinearMipmapLinearFilter 1008;
/// RepeatWrapping 1000, ClampToEdgeWrapping 1001, MirroredRepeatWrapping 1002).
pub(super) fn sampler(dev: &Device, m: &TextureMeta) -> Result<Sampler, String> {
    let d = MTLSamplerDescriptor::new();
    let lin = |f: u32| if matches!(f, 1006..=1008) { MTLSamplerMinMagFilter::Linear } else { MTLSamplerMinMagFilter::Nearest };
    let wrap = |w: u32| match w {
        1001 => MTLSamplerAddressMode::ClampToEdge,
        1002 => MTLSamplerAddressMode::MirrorRepeat,
        _ => MTLSamplerAddressMode::Repeat,
    };
    d.setMagFilter(lin(m.mag_filter));
    d.setMinFilter(lin(m.min_filter));
    d.setMipFilter(match m.min_filter {
        1004 | 1007 => MTLSamplerMipFilter::Nearest,
        1005 | 1008 => MTLSamplerMipFilter::Linear,
        _ => MTLSamplerMipFilter::NotMipmapped,
    });
    d.setSAddressMode(wrap(m.wrap[0]));
    d.setTAddressMode(wrap(m.wrap[1]));
    d.setMaxAnisotropy(m.anisotropy.clamp(1, 16) as usize);
    dev.newSamplerStateWithDescriptor(&d).ok_or_else(|| "sampler".into())
}

impl ThreeScene {
    pub(super) fn apply(&mut self, dev: &Device, res: Vec<Resource>) {
        for r in res {
            match r {
                Resource::Release { kind: k, id } => {
                    // bind lists name textures by id: a texture or attachment that goes or comes rebuilds them
                    if k == kind::TEXTURE || k == kind::RENDER_TARGET {
                        self.bind_arena.invalidate();
                    }
                    match k {
                        kind::GEOMETRY => {
                            if let Some(g) = self.geos.remove(&id) {
                                self.cl.retire(g.cl, self.clock.encoding);
                                g.bury(&mut self.dead);
                            }
                        }
                        kind::TEXTURE => {
                            let old = self.texs.remove(&id);
                            self.bury_texture(old);
                        }
                        kind::INSTANCES => {
                            if let Some(i) = self.insts.remove(&id) {
                                i.bury(&mut self.dead);
                            }
                        }
                        kind::MATERIAL => { self.mats3.remove(&id); }
                        kind::RENDER_TARGET => self.targets.release(id),
                        _ => {}
                    }
                }
                Resource::Upload { kind: k, id, meta, arrays } => {
                    // a texture re-uploaded as the same kind of texture (new pixels) changes no bind list: they
                    // name it by id. A new one, or one that changed type, does; so does any render target
                    let shape = (k == kind::TEXTURE).then(|| self.texture_shape(id));
                    let ok = match k {
                        kind::GEOMETRY => self.upload_geometry(dev, id, &meta, &arrays),
                        kind::TEXTURE if meta.contains("\"framebuffer\":true") => self.targets.framebuffer_texture(dev, id, &meta),
                        kind::TEXTURE => self.upload_texture(dev, id, &meta, &arrays),
                        kind::RENDER_TARGET => self.targets.upload(dev, id, &meta),
                        kind::MATERIAL if meta.contains("\"prewarm\":") => self.prewarm_material(id, &meta),
                        kind::MATERIAL if meta.contains("\"program\":") => self.upload_program_material(dev, id, &meta),
                        kind::MATERIAL => self.upload_material(id, &meta),
                        kind::INSTANCES => self.upload_instances(dev, id, &meta, &arrays),
                        _ => Err(format!("unknown resource kind {k}")),
                    };
                    if k == kind::RENDER_TARGET || shape.is_some_and(|s| s != self.texture_shape(id)) {
                        self.bind_arena.invalidate();
                    }
                    if let Err(e) = ok {
                        self.unsupported_resources += 1;
                        eprintln!("three upload {k}/{id}: {e}");
                    }
                }
            }
        }
        if self.mat_dirty {
            // a fresh buffer: frames in flight read the previous one, which waits in the graveyard for them
            let old = std::mem::replace(&mut self.mat_buf, buffer_with(dev, &self.mat_rows));
            self.bury(Dead::Buffer(old));
            self.mat_dirty = false;
        }
    }

    /// A texture entry taken out of the registry (replaced or released).
    fn bury_texture(&mut self, old: Option<(Texture, TextureMeta, Sampler)>) {
        if let Some((t, _, s)) = old {
            self.dead.push(Dead::Texture(t));
            self.dead.push(Dead::Sampler(s));
        }
    }

    fn upload_geometry(&mut self, dev: &Device, id: u32, meta: &str, a: &[Option<Array>]) -> Result<(), String> {
        let m: GeometryMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let pos = f32s(a.first().unwrap_or(&None)).ok_or("no positions")?;
        let nrm = f32s(a.get(1).unwrap_or(&None)).ok_or("no normals")?;
        if pos.len() < m.vertices * 3 || nrm.len() < m.vertices * 3 {
            return Err("short position/normal stream".into());
        }
        let index_count = match a.get(4).unwrap_or(&None) {
            Some(Array::U16(v)) => v.len() as u32,
            Some(Array::U32(v)) => v.len() as u32,
            _ => 0,
        };
        // how often this id was uploaded before: a geometry that keeps changing gets no cluster LOD
        let cl_uploads = self.geos.get(&id).map_or(0, |g| g.cl_uploads.saturating_add(1));
        let index = match a.get(4).unwrap_or(&None) {
            Some(Array::U16(v)) => Some((buffer_with(dev, v), MTLIndexType::UInt16)),
            Some(Array::U32(v)) => Some((buffer_with(dev, v), MTLIndexType::UInt32)),
            _ => None,
        };
        let (pos, nrm) = (buffer_with(dev, pos), buffer_with(dev, nrm));
        let uv = f32s(a.get(2).unwrap_or(&None)).map(|v| buffer_with(dev, v));
        let col = f32s(a.get(3).unwrap_or(&None)).map(|v| buffer_with(dev, v));
        // every attribute by name, for three's programs (the v2 shader reads the four fixed streams)
        let mut attrs = vec![(self.progs.name_id("position"), pos.clone(), 3), (self.progs.name_id("normal"), nrm.clone(), 3)];
        if let Some(b) = &uv {
            attrs.push((self.progs.name_id("uv"), b.clone(), 2));
        }
        if let Some(b) = &col {
            attrs.push((self.progs.name_id("color"), b.clone(), m.color_size.clamp(3, 4)));
        }
        let (mut inst_attrs, mut inst_cap) = (Vec::new(), None::<usize>);
        for (k, am) in m.attributes.iter().enumerate() {
            let data = f32s(a.get(5 + k).unwrap_or(&None)).ok_or_else(|| format!("attribute {}: no f32 data", am.name))?;
            if am.per_instance {
                // one element per instance (meshPerAttribute > 1 is expanded by the JS side)
                let n = data.len() / am.item_size.max(1) as usize;
                if !(1..=4).contains(&am.item_size) || n == 0 {
                    return Err(format!("instanced attribute {}: {} floats of {}", am.name, data.len(), am.item_size));
                }
                inst_attrs.push((self.progs.name_id(&am.name), buffer_with(dev, data), am.item_size));
                inst_cap = Some(inst_cap.map_or(n, |c| c.min(n)));
                continue;
            }
            if !(1..=4).contains(&am.item_size) || data.len() < m.vertices * am.item_size as usize {
                return Err(format!("attribute {}: {} floats for {} vertices of {}", am.name, data.len(), m.vertices, am.item_size));
            }
            attrs.push((self.progs.name_id(&am.name), buffer_with(dev, data), am.item_size));
        }
        let old = self.geos.insert(
            id,
            Geo {
                plans: Vec::new(),
                attrs,
                inst_attrs,
                inst_cap,
                pos,
                nrm,
                uv,
                col,
                color_size: m.color_size,
                index,
                sphere: m.sphere,
                counts: (m.vertices as u32, index_count),
                cl: GeoCl::None,
                cl_uploads,
            },
        );
        if let Some(g) = old {
            self.cl.retire(g.cl, self.clock.encoding);
            g.bury(&mut self.dead);
        }
        Ok(())
    }

    fn upload_texture(&mut self, dev: &Device, id: u32, meta: &str, a: &[Option<Array>]) -> Result<(), String> {
        let m: TextureMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        if m.compressed {
            let t = compressed_texture(dev, &m, a)?;
            let s = sampler(dev, &m)?;
            let old = self.texs.insert(id, (t, m, s));
            self.bury_texture(old);
            return Ok(());
        }
        let px = match a.first().unwrap_or(&None) {
            Some(Array::U8(v)) => v,
            _ => return Err("texture pixels must come as bytes".into()),
        };
        let (channels, bytes) = m.texel().ok_or_else(|| format!("texture format {} type {} not supported", m.format, m.ty))?;
        let bpp = channels * bytes;
        // DataArrayTexture (dim 2: morph targets in r187) and Data3DTexture (dim 3): `depth` layers / slices
        let layers = if m.dim == 2 || m.dim == 3 { m.depth.max(1) } else { 1 };
        let image = m.width * m.height * bpp;
        if px.len() < image * layers {
            return Err(format!("texture {}x{}x{layers} needs {} bytes, got {}", m.width, m.height, image * layers, px.len()));
        }
        let fmt = match (channels, bytes, m.integer()) {
            (4, 4, Some(true)) => MTLPixelFormat::RGBA32Uint,
            (2, 4, Some(true)) => MTLPixelFormat::RG32Uint,
            (_, 4, Some(true)) => MTLPixelFormat::R32Uint,
            (4, 4, Some(false)) => MTLPixelFormat::RGBA32Sint,
            (2, 4, Some(false)) => MTLPixelFormat::RG32Sint,
            (_, 4, Some(false)) => MTLPixelFormat::R32Sint,
            (4, 1, _) if m.srgb => MTLPixelFormat::RGBA8Unorm_sRGB,
            (4, 1, _) => MTLPixelFormat::RGBA8Unorm,
            (2, 1, _) => MTLPixelFormat::RG8Unorm,
            (1, 1, _) => MTLPixelFormat::R8Unorm,
            (4, 2, _) => MTLPixelFormat::RGBA16Float,
            (2, 2, _) => MTLPixelFormat::RG16Float,
            (1, 2, _) => MTLPixelFormat::R16Float,
            (4, 4, _) => MTLPixelFormat::RGBA32Float,
            (2, 4, _) => MTLPixelFormat::RG32Float,
            _ => MTLPixelFormat::R32Float,
        };
        // WebGL's UNPACK_FLIP_Y (v3 asks for it: three's shaders sample the texture as stored), per layer
        let flipped: Vec<u8>;
        let px: &[u8] = if m.flip_rows {
            let row = m.width * bpp;
            flipped = px[..image * layers].chunks_exact(image).flat_map(|l| l.chunks_exact(row).rev().flatten().copied()).collect();
            &flipped
        } else {
            px
        };
        let t = if m.dim == 2 || m.dim == 3 {
            let d = MTLTextureDescriptor::new();
            d.setTextureType(if m.dim == 3 { MTLTextureType::Type3D } else { MTLTextureType::Type2DArray });
            d.setPixelFormat(fmt);
            unsafe {
                d.setWidth(m.width);
                d.setHeight(m.height);
                if m.dim == 3 {
                    d.setDepth(layers);
                } else {
                    d.setArrayLength(layers);
                }
            }
            d.setUsage(MTLTextureUsage::ShaderRead);
            let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
            unsafe {
                if m.dim == 3 {
                    t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                        MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: layers } },
                        0,
                        0,
                        NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                        m.width * bpp,
                        image,
                    );
                } else {
                    for l in 0..layers {
                        t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                            MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: 1 } },
                            0,
                            l,
                            NonNull::new(px[l * image..].as_ptr() as *mut c_void).unwrap(),
                            m.width * bpp,
                            0,
                        );
                    }
                }
            }
            t
        } else {
            // three: mips only when generateMipmaps and the min filter uses them (DataTexture defaults to neither)
            let mips = m.mipmaps && m.min_filter_mips() && m.width > 1 && m.height > 1 && bpp == 4 && bytes == 1;
            let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, m.width, m.height, mips) };
            d.setUsage(MTLTextureUsage::ShaderRead);
            let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
            unsafe {
                t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: 1 } },
                    0,
                    NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                    m.width * bpp,
                );
            }
            if mips {
                // generated by the frame this update belongs to, before any pass samples it (`encode_uploads`)
                self.mip_queue.push(t.clone());
            }
            t
        };
        // materials referencing this texture carry its transform; refresh them
        for mat in self.mats.values() {
            if mat.meta.map == id {
                let row = &mut self.mat_rows[mat.row as usize];
                row.map_xform = [m.repeat[0], m.repeat[1], m.offset[0], m.offset[1]];
                row.misc[0] = m.flip_y as u32;
                self.mat_dirty = true;
            }
        }
        let s = sampler(dev, &m)?;
        let old = self.texs.insert(id, (t, m, s));
        self.bury_texture(old);
        Ok(())
    }

    fn upload_material(&mut self, id: u32, meta: &str) -> Result<(), String> {
        let m: MaterialMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let row = ThreeMat {
            color: [m.color[0], m.color[1], m.color[2], m.opacity],
            emissive_rough: [m.emissive[0], m.emissive[1], m.emissive[2], m.roughness.clamp(0.03, 1.0)],
            // unknown models (ShaderMaterial etc., counted on the JS side) fall back to standard
            params: [m.metalness, (if m.model == 0 { 3 } else { m.model }) as f32, m.alpha_test, (m.map != 0) as u32 as f32],
            map_xform: [1.0, 1.0, 0.0, 0.0],
            misc: [0, m.flags, 0, 0],
        };
        // textures usually upload before the material that uses them: take the transform now
        let mut row = row;
        if let Some((_, tm, _)) = self.texs.get(&m.map) {
            row.map_xform = [tm.repeat[0], tm.repeat[1], tm.offset[0], tm.offset[1]];
            row.misc[0] = tm.flip_y as u32;
        }
        let idx = match self.mats.get(&id) {
            Some(existing) => existing.row,
            None => {
                self.mat_rows.push(row);
                (self.mat_rows.len() - 1) as u32
            }
        };
        self.mat_rows[idx as usize] = row;
        self.mats.insert(id, Mat { row: idx, meta: m });
        self.mat_dirty = true;
        Ok(())
    }

    fn upload_instances(&mut self, dev: &Device, id: u32, meta: &str, a: &[Option<Array>]) -> Result<(), String> {
        let m: InstancesMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let data = f32s(a.first().unwrap_or(&None)).ok_or("no instance matrices")?;
        let colors = f32s(a.get(1).unwrap_or(&None)).map(|v| buffer_with(dev, v));
        if m.mat4 {
            // v3: three's `instanceMatrix.array` as it is (column-major 4x4 per instance), which is the layout the
            // translated programs read: one buffer, no conversion. Only v3 pipelines draw these (`rows` is the v2
            // layout; a v3 renderer never binds it, so it shares the buffer).
            if data.len() < m.count * 16 {
                return Err("short instance matrices".into());
            }
            let cols = if m.count == 0 { buffer_with(dev, &[0.0f32; 16]) } else { buffer_with(dev, &data[..m.count * 16]) };
            if let Some(old) = self.insts.insert(id, Inst { rows: cols.clone(), cols, colors, count: m.count, hint: m.cull, cull: None }) {
                old.bury(&mut self.dead);
            }
            return Ok(());
        }
        let rows = data;
        if rows.len() < m.count * 12 {
            return Err("short instance rows".into());
        }
        let mut cols = Vec::with_capacity(m.count.max(1) * 16);
        for r in rows[..m.count * 12].chunks_exact(12) {
            for c in 0..4 {
                cols.extend_from_slice(&[r[c], r[4 + c], r[8 + c], if c == 3 { 1.0 } else { 0.0 }]);
            }
        }
        if cols.is_empty() {
            cols.extend_from_slice(&[0.0; 16]);
        }
        if let Some(old) = self.insts.insert(id, Inst { rows: buffer_with(dev, &rows[..m.count * 12]), cols: buffer_with(dev, &cols), colors, count: m.count, hint: m.cull, cull: None }) {
            old.bury(&mut self.dead);
        }
        Ok(())
    }

    /// MATERIAL `{ prewarm, token }` (docs/PROTOCOL.md): program `prewarm` will take the place of the program
    /// instance `id` is drawn with. Its pipelines for the streams and targets the instance is drawn with now are
    /// queued (the two programs must take the same vertex inputs: a stream signature is per attribute list; they do
    /// when only the light counts differ). `token` != 0: every note up to it has been applied.
    fn prewarm_material(&mut self, id: u32, meta: &str) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct Note {
            prewarm: u32,
            #[serde(default)]
            token: u32,
        }
        let n: Note = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        if let Some(m) = self.mats3.get(&id).filter(|_| n.prewarm != 0) {
            let same_inputs = match (self.progs.progs.get(&m.meta.program), self.progs.progs.get(&n.prewarm)) {
                (Some(a), Some(b)) => a.attrs == b.attrs,
                _ => false,
            };
            if same_inputs {
                let meta = ProgramMaterialMeta { program: n.prewarm, ..m.meta.clone() };
                let pipes: Vec<(StreamSig, Target)> = m.pipes.iter().filter(|p| p.2 != u32::MAX).map(|p| (p.0, p.1)).collect();
                for (sig, target) in pipes {
                    self.progs.prewarm(&meta, sig, target);
                }
            }
        }
        if n.token != 0 {
            self.progs.prewarm_token(n.token);
        }
        Ok(())
    }

    fn upload_program_material(&mut self, dev: &Device, id: u32, meta: &str) -> Result<(), String> {
        let m: ProgramMaterialMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let depth = self.progs.depth(dev, &m);
        let state = Mat3State::new(m, depth, self.mats3.get(&id), &self.progs);
        self.mats3.insert(id, state);
        Ok(())
    }
}

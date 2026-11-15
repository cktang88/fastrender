//! Native voxel world: generation, parallel meshing with voxel-castle-defense's own greedy mesher
//! (`voxel-core`, the same Rust that the game runs as WASM), and packing into 8-byte GPU vertices.
//! Portable (tested on any OS); the Metal side lives in `mac.rs`.

use glam::Vec3;
use rayon::prelude::*;
use voxel_core::{Mesher, BLOCK_STRIDE, CHUNK};

use crate::core::Rng;

// block ids, as in voxel-castle-defense src/blocks.ts
pub const AIR: u8 = 0;
pub const GRASS: u8 = 1;
pub const DIRT: u8 = 2;
pub const STONE: u8 = 3;
pub const SAND: u8 = 4;
pub const BEDROCK: u8 = 5;
pub const LOG: u8 = 6;
pub const LEAVES: u8 = 7;
pub const PLANKS: u8 = 8;
pub const BRICK: u8 = 9;
pub const ROOF: u8 = 16;
pub const SNOW: u8 = 19;
pub const LANTERN: u8 = 13;
pub const COBBLE: u8 = 14;

fn srgb(hex: u32) -> [f32; 3] {
    let c = |v: u32| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    [c(hex >> 16 & 255), c(hex >> 8 & 255), c(hex & 255)]
}

/// The game's block table (`blockTable()` in meshing.ts): top, side, side-top, bottom colours
/// (linear), emit, anchored, detail tiles. Colours copied from src/blocks.ts.
pub fn block_table() -> Vec<f32> {
    // (id, top, side, side_top, bottom, emit, anchored)
    let defs: &[(u8, u32, Option<u32>, Option<u32>, Option<u32>, f32, bool)] = &[
        (GRASS, 0x8fc45a, Some(0x8a6643), Some(0x6fa44a), Some(0x7a5a3a), 0.0, true),
        (DIRT, 0x8f6a45, None, None, None, 0.0, true),
        (STONE, 0x9c9a94, Some(0x8a8781), None, None, 0.0, true),
        (SAND, 0xe8d39a, Some(0xdcc58a), None, None, 0.0, true),
        (BEDROCK, 0x3a3a44, None, None, None, 0.0, true),
        (LOG, 0xb88a55, Some(0x6e4a2c), None, None, 0.0, false),
        (LEAVES, 0x5fa845, Some(0x4f9440), None, None, 0.0, false),
        (PLANKS, 0xc9955a, Some(0xb8844c), None, None, 0.0, false),
        (BRICK, 0xb9b2a4, Some(0xa8705a), None, None, 0.0, false),
        (LANTERN, 0xffd27a, Some(0xffc35a), None, None, 1.0, false),
        (COBBLE, 0x8e8b84, Some(0x7d7a73), None, None, 0.0, false),
        (ROOF, 0xb04a3a, Some(0x9a3e30), None, None, 0.0, false),
        (SNOW, 0xf2f5fa, Some(0xdfe5ee), None, None, 0.0, true),
    ];
    let mut t = vec![0.0f32; 23 * BLOCK_STRIDE];
    for &(id, top, side, side_top, bottom, emit, anchored) in defs {
        let o = id as usize * BLOCK_STRIDE;
        let side = side.unwrap_or(top);
        t[o..o + 3].copy_from_slice(&srgb(top));
        t[o + 3..o + 6].copy_from_slice(&srgb(side));
        match side_top {
            Some(c) => t[o + 6..o + 9].copy_from_slice(&srgb(c)),
            None => t[o + 6] = -1.0,
        }
        t[o + 9..o + 12].copy_from_slice(&srgb(bottom.unwrap_or(side)));
        t[o + 12] = emit;
        t[o + 13] = if anchored { 1.0 } else { 0.0 };
    }
    t
}

/// Dense voxel volume, x fastest, then z, then y (the mesher's layout).
pub struct World {
    pub w: usize,
    pub h: usize,
    pub d: usize,
    pub vox: Vec<u8>,
    pub height: Vec<u16>, // surface height per column
    pub lanterns: Vec<[i32; 3]>,
}

impl World {
    #[inline]
    pub fn idx(&self, x: usize, y: usize, z: usize) -> usize { x + z * self.w + y * self.w * self.d }
    pub fn get(&self, x: i32, y: i32, z: i32) -> u8 {
        if x < 0 || z < 0 || y < 0 || x >= self.w as i32 || z >= self.d as i32 || y >= self.h as i32 {
            return AIR;
        }
        self.vox[self.idx(x as usize, y as usize, z as usize)]
    }
    fn set(&mut self, x: i32, y: i32, z: i32, b: u8) {
        if x >= 0 && z >= 0 && y >= 0 && (x as usize) < self.w && (z as usize) < self.d && (y as usize) < self.h {
            let i = self.idx(x as usize, y as usize, z as usize);
            self.vox[i] = b;
        }
    }
    pub fn surface(&self, x: f32, z: f32) -> f32 {
        let xi = (x.max(0.0) as usize).min(self.w - 1);
        let zi = (z.max(0.0) as usize).min(self.d - 1);
        self.height[xi + zi * self.w] as f32
    }
}

fn value_noise(seed: u64, x: f32, z: f32) -> f32 {
    let h = |ix: i32, iz: i32| {
        let mut r = Rng::new(seed ^ ((ix as u64) << 32 | (iz as u32 as u64)).wrapping_mul(0x9E3779B97F4A7C15));
        r.f32()
    };
    let (x0, z0) = (x.floor(), z.floor());
    let (fx, fz) = (x - x0, z - z0);
    let (sx, sz) = (fx * fx * (3.0 - 2.0 * fx), fz * fz * (3.0 - 2.0 * fz));
    let (ix, iz) = (x0 as i32, z0 as i32);
    let a = h(ix, iz) + (h(ix + 1, iz) - h(ix, iz)) * sx;
    let b = h(ix, iz + 1) + (h(ix + 1, iz + 1) - h(ix, iz + 1)) * sx;
    a + (b - a) * sz
}

fn fbm(seed: u64, x: f32, z: f32) -> f32 {
    let mut s = 0.0;
    let mut amp = 0.5;
    let mut f = 1.0;
    for o in 0..5 {
        s += value_noise(seed + o, x * f, z * f) * amp;
        amp *= 0.5;
        f *= 2.0;
    }
    s
}

/// Island terrain with hills, beaches, snow caps, trees and small brick/plank houses: the kind of
/// content Blockfall Isle has, sized to stress meshing and rendering. Not the game's own worldgen.ts.
pub fn generate(chunks_x: usize, chunks_y: usize, chunks_z: usize, seed: u64) -> World {
    let (w, h, d) = (chunks_x * CHUNK, chunks_y * CHUNK, chunks_z * CHUNK);
    let mut world = World { w, h, d, vox: vec![AIR; w * h * d], height: vec![0; w * d], lanterns: Vec::new() };
    let sea = (h as f32 * 0.22) as i32;
    // columns in parallel: each z row writes a disjoint set of indices only through `heights`
    let heights: Vec<u16> = (0..w * d)
        .into_par_iter()
        .map(|i| {
            let (x, z) = ((i % w) as f32, (i / w) as f32);
            let (nx, nz) = (x / w as f32 - 0.5, z / d as f32 - 0.5);
            let island = (1.0 - (nx * nx + nz * nz).sqrt() * 2.2).clamp(0.0, 1.0);
            let hills = fbm(seed, x / 90.0, z / 90.0);
            let ridge = 1.0 - (fbm(seed + 17, x / 140.0, z / 140.0) * 2.0 - 1.0).abs();
            let y = sea as f32 - 6.0 + island * (hills * h as f32 * 0.55 + ridge * ridge * h as f32 * 0.25);
            y.clamp(2.0, h as f32 - 8.0) as u16
        })
        .collect();
    for z in 0..d {
        for x in 0..w {
            let top = heights[x + z * w] as i32;
            for y in 0..=top {
                let b = if y == 0 {
                    BEDROCK
                } else if y < top - 4 {
                    STONE
                } else if top <= sea + 1 {
                    SAND
                } else if top > (h as f32 * 0.68) as i32 {
                    if y == top { SNOW } else { STONE }
                } else if y == top {
                    GRASS
                } else {
                    DIRT
                };
                world.set(x as i32, y, z as i32, b);
            }
        }
    }
    world.height = heights;
    // trees and houses
    let mut rng = Rng::new(seed ^ 0xA11CE);
    let n_trees = w * d / 160;
    for _ in 0..n_trees {
        let (x, z) = ((rng.f32() * w as f32) as i32, (rng.f32() * d as f32) as i32);
        let top = world.surface(x as f32, z as f32) as i32;
        if world.get(x, top, z) != GRASS { continue; }
        let th = 4 + (rng.f32() * 4.0) as i32;
        for y in 1..=th { world.set(x, top + y, z, LOG); }
        let r = 2 + (rng.f32() * 1.5) as i32;
        for dy in -r..=r {
            for dz in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy + dz * dz <= r * r + 1 && world.get(x + dx, top + th + dy, z + dz) == AIR {
                        world.set(x + dx, top + th + dy, z + dz, LEAVES);
                    }
                }
            }
        }
    }
    let n_houses = w * d / 2500;
    for _ in 0..n_houses {
        let (x, z) = ((rng.f32() * w as f32) as i32, (rng.f32() * d as f32) as i32);
        let top = world.surface(x as f32, z as f32) as i32;
        if world.get(x, top, z) != GRASS { continue; }
        let (sx, sz, sh) = (5 + (rng.f32() * 6.0) as i32, 5 + (rng.f32() * 6.0) as i32, 4 + (rng.f32() * 3.0) as i32);
        let wall = if rng.f32() < 0.5 { BRICK } else { PLANKS };
        for y in 1..=sh {
            for dz in 0..sz {
                for dx in 0..sx {
                    let edge = dx == 0 || dz == 0 || dx == sx - 1 || dz == sz - 1;
                    let door = dz == 0 && dx == sx / 2 && y <= 2;
                    let window = edge && y == 2 && (dx + dz) % 3 == 1;
                    world.set(x + dx, top + y, z + dz, if !edge || door || window { AIR } else { wall });
                }
            }
        }
        for dz in 0..sz { for dx in 0..sx { world.set(x + dx, top, z + dz, COBBLE); } }
        // stepped roof
        for k in 0..=(sx / 2) {
            for dz in -1..=sz {
                world.set(x + k, top + sh + 1 + k, z + dz, ROOF);
                world.set(x + sx - 1 - k, top + sh + 1 + k, z + dz, ROOF);
            }
        }
        world.set(x + sx / 2, top + 3, z - 1, LANTERN);
        world.lanterns.push([x + sx / 2, top + 3, z - 1]);
    }
    world
}

/// 8-byte packed vertex: see `unpack` and `vs_voxel` in the shader.
///   w0: x 6b | y 6b << 6 | z 6b << 12 | face dir 3b << 18 | emit 7b << 21
///   w1: linear rgb 10:10:10 (AO and damage already baked by the mesher)
pub type PackedVertex = [u32; 2];

#[derive(Clone, Copy, Debug, Default)]
pub struct ChunkMesh {
    pub origin: [i32; 3],
    pub base_vertex: u32,
    pub first_index: u32,
    pub index_count: u32,
}

pub struct VoxelMeshes {
    pub vertices: Vec<PackedVertex>,
    pub indices: Vec<u32>,
    pub chunks: Vec<ChunkMesh>,
    pub mesh_ms: f64,
    pub chunks_meshed: usize,
}

fn dir_of(n: [i8; 3]) -> u32 {
    match n {
        [1, 0, 0] => 0,
        [-1, 0, 0] => 1,
        [0, 1, 0] => 2,
        [0, -1, 0] => 3,
        [0, 0, 1] => 4,
        _ => 5,
    }
}

pub fn pack(p: [u8; 3], n: [i8; 3], c: [u16; 3], emit: u8) -> PackedVertex {
    let q = |v: u16| (v as u32 * 1023 + 32767) / 65535;
    [
        p[0] as u32 | (p[1] as u32) << 6 | (p[2] as u32) << 12 | dir_of(n) << 18 | ((emit as u32) >> 1) << 21,
        q(c[0]) | q(c[1]) << 10 | q(c[2]) << 20,
    ]
}

pub fn unpack_pos(v: PackedVertex) -> [u32; 3] { [v[0] & 63, (v[0] >> 6) & 63, (v[0] >> 12) & 63] }

/// Mesh every chunk in parallel (one Mesher per worker thread) and pack into shared buffers.
pub fn mesh_world(world: &World) -> VoxelMeshes {
    const P: usize = CHUNK + 2;
    let table = block_table();
    let (cx_n, cy_n, cz_n) = (world.w / CHUNK, world.h / CHUNK, world.d / CHUNK);
    let coords: Vec<(i32, i32, i32)> = (0..cy_n).flat_map(|y| (0..cz_n).flat_map(move |z| (0..cx_n).map(move |x| (x as i32, y as i32, z as i32)))).collect();
    let t0 = std::time::Instant::now();
    let zero = vec![0u8; CHUNK * CHUNK * CHUNK];
    let per_chunk: Vec<(ChunkMesh, Vec<PackedVertex>, Vec<u32>)> = coords
        .par_iter()
        .map_init(
            || {
                let mut m = Mesher::new();
                m.set_blocks(&table);
                (m, vec![0u8; P * P * P])
            },
            |(m, vox), &(cx, cy, cz)| {
                let (x0, y0, z0) = (cx * CHUNK as i32, cy * CHUNK as i32, cz * CHUNK as i32);
                let mut any = false;
                for py in 0..P {
                    for pz in 0..P {
                        for px in 0..P {
                            let b = world.get(x0 + px as i32 - 1, y0 + py as i32 - 1, z0 + pz as i32 - 1);
                            any |= b != AIR;
                            vox[px + pz * P + py * P * P] = b;
                        }
                    }
                }
                let origin = [x0, y0, z0];
                if !any { return (ChunkMesh { origin, ..Default::default() }, Vec::new(), Vec::new()); }
                let n = m.mesh(vox, &zero, &zero, x0, y0, z0);
                let (pos, nor, col, emit, ind) = unsafe {
                    (
                        std::slice::from_raw_parts(m.pos_ptr(), n * 3),
                        std::slice::from_raw_parts(m.nor_ptr(), n * 3),
                        std::slice::from_raw_parts(m.col_ptr(), n * 3),
                        std::slice::from_raw_parts(m.emit_ptr(), n),
                        std::slice::from_raw_parts(m.ind_ptr(), m.ind_len()),
                    )
                };
                let verts = (0..n)
                    .map(|i| pack([pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]], [nor[i * 3], nor[i * 3 + 1], nor[i * 3 + 2]], [col[i * 3], col[i * 3 + 1], col[i * 3 + 2]], emit[i]))
                    .collect();
                (ChunkMesh { origin, ..Default::default() }, verts, ind.to_vec())
            },
        )
        .collect();
    let mesh_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut out = VoxelMeshes { vertices: Vec::new(), indices: Vec::new(), chunks: Vec::new(), mesh_ms, chunks_meshed: coords.len() };
    for (mut c, v, i) in per_chunk {
        if i.is_empty() { continue; }
        c.base_vertex = out.vertices.len() as u32;
        c.first_index = out.indices.len() as u32;
        c.index_count = i.len() as u32;
        out.vertices.extend_from_slice(&v);
        out.indices.extend_from_slice(&i);
        out.chunks.push(c);
    }
    out
}

/// Fly-over camera: follows a loop around the island at a fixed height above the terrain.
pub fn camera_at(world: &World, t: f32) -> (Vec3, Vec3) {
    let (cx, cz) = (world.w as f32 * 0.5, world.d as f32 * 0.5);
    let r = world.w.min(world.d) as f32 * 0.28;
    let a = t * 0.05;
    let p = |a: f32| Vec3::new(cx + a.cos() * r, 0.0, cz + a.sin() * r);
    let (mut pos, ahead) = (p(a), p(a + 0.08));
    pos.y = world.surface(pos.x, pos.z).max(world.surface(ahead.x, ahead.z)) + 14.0;
    let target = Vec3::new(ahead.x, world.surface(ahead.x, ahead.z) + 4.0, ahead.z);
    (pos, (target - pos).normalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        let v = pack([32, 17, 0], [0, -1, 0], [65535, 0, 32768], 255);
        assert_eq!(unpack_pos(v), [32, 17, 0]);
        assert_eq!((v[0] >> 18) & 7, 3);
        assert_eq!(v[1] & 1023, 1023);
        assert_eq!((v[1] >> 10) & 1023, 0);
        assert!(((v[1] >> 20) & 1023).abs_diff(512) <= 1);
        assert_eq!(v[0] >> 21, 127);
    }

    #[test]
    fn world_meshes_into_valid_buffers() {
        let world = generate(4, 2, 4, 3);
        assert!(world.vox.iter().any(|&b| b == GRASS) && world.vox.iter().any(|&b| b == LEAVES));
        let m = mesh_world(&world);
        assert!(!m.chunks.is_empty());
        assert!(m.vertices.len() > 1000, "only {} vertices", m.vertices.len());
        for c in &m.chunks {
            let end = c.base_vertex as usize;
            let next = m.chunks.iter().map(|o| o.base_vertex as usize).filter(|&b| b > end).min().unwrap_or(m.vertices.len());
            for &i in &m.indices[c.first_index as usize..(c.first_index + c.index_count) as usize] {
                assert!(end + (i as usize) < next, "index out of its chunk's vertex range");
            }
        }
        assert!(m.vertices.iter().all(|&v| unpack_pos(v).iter().all(|&p| p <= CHUNK as u32)));
    }

    #[test]
    fn camera_above_ground() {
        let world = generate(4, 2, 4, 3);
        for i in 0..200 {
            let (p, d) = camera_at(&world, i as f32);
            assert!(p.y > world.surface(p.x, p.z));
            assert!((d.length() - 1.0).abs() < 1e-3);
        }
    }
}

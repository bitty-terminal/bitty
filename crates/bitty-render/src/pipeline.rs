//! GPU presentation resources: pipelines, shaders, atlas textures.
//!
//! This module is the `wgpu`-dependent half of presentation. It consumes the
//! bounded batches from [`crate::batch`] and owns the long-lived GPU objects
//! (render pipelines, vertex/index buffers, atlas + inline textures,
//! samplers, bind groups). Everything here is `pub(crate)`: no `wgpu` type
//! appears in the crate's public API (ADR-0004 "Adopt" row), and every
//! failure is flattened into the owned [`RenderError`](crate::RenderError).
//!
//! # Resources and lifecycle
//!
//! [`GpuResources`] is created lazily on the first presented frame for the
//! surface's negotiated [`TextureFormat`](wgpu::TextureFormat) and the
//! renderer's [`AtlasDims`](crate::atlas::AtlasDims), then reused across
//! frames. It is recreated (fail-safe, no panic) when the surface format or
//! the atlas dimensions change, which covers swap-chain reconfiguration and
//! device-loss recovery driven by the caller (`Outdated`/`Lost` reconfigure
//! plus one retry in [`Surface::present_draw_list`](crate::gpu::Surface)).
//!
//! # Bounded invariants
//!
//! Vertex buffers are fixed at the [`crate::batch`] chunk caps
//! ([`MAX_FILL_QUADS_PER_BATCH`](crate::batch::MAX_FILL_QUADS_PER_BATCH) and
//! [`MAX_GLYPH_QUADS_PER_BATCH`](crate::batch::MAX_GLYPH_QUADS_PER_BATCH));
//! frames larger than one chunk draw chunk after chunk reusing the same
//! buffers. The atlas texture is capped by
//! [`MAX_ATLAS_DIMENSION`](crate::batch::MAX_ATLAS_DIMENSION) and the
//! transient inline texture is fixed at
//! [`INLINE_TEXTURE_SIZE`](crate::batch::INLINE_TEXTURE_SIZE). Every
//! `write_buffer`/`write_texture` call is bounds-checked first and maps
//! overruns to [`RenderError::InvalidInput`] (fail-closed) instead of
//! panicking.
//!
//! # Shaders
//!
//! Two minimal WGSL programs: a solid-fill pass (`pos + color`) and an
//! atlas-textured glyph pass (`pos + uv + color`, coverage from the `R8`
//! texture's red channel modulating the tint alpha). Both pipelines enable
//! standard alpha blending so the cursor (semi-transparent white) and faint
//! text composite over backgrounds. Gamma note: tint bytes are normalized
//! `sRGB` values written to an `sRGB` target without an explicit
//! linear-space round trip; output is slightly brighter than a fully
//! color-managed pipeline. Text stays clearly legible (the P0 fix is
//! visibility, not gamma exactness); a linear-workflow follow-up can add
//! the conversion without changing any batch layout.

use wgpu::{
    AddressMode, BindGroup, BindGroupLayout, BlendState, Buffer, BufferDescriptor, BufferUsages,
    ColorTargetState, ColorWrites, Extent3d, FilterMode, FragmentState, IndexFormat, LoadOp,
    MultisampleState, Operations, Origin3d, PipelineLayoutDescriptor, PrimitiveState,
    RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor,
    Sampler, SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor, ShaderSource,
    TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    VertexAttribute, VertexBufferLayout, VertexFormat, VertexState, VertexStepMode,
};

use crate::atlas::AtlasDims;
use crate::batch::{
    self, AtlasDirty, FILL_VERTEX_SIZE_BYTES, GLYPH_VERTEX_SIZE_BYTES, INDICES_PER_QUAD,
    MAX_FILL_QUADS_PER_BATCH, MAX_GLYPH_QUADS_PER_BATCH, VERTICES_PER_QUAD,
};
use crate::error::RenderError;
use crate::grid::DrawList;

/// Solid-fill WGSL: NDC position plus straight color passthrough.
const FILL_WGSL: &str = r#"
struct FillOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
) -> FillOut {
    var out: FillOut;
    out.pos = vec4<f32>(position, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> {
    return color;
}
"#;

/// Glyph WGSL: samples the R8 coverage atlas and modulates tint alpha.
const GLYPH_WGSL: &str = r#"
@group(0) @binding(0) var atlas_tex: texture_2d<f32>;
@group(0) @binding(1) var atlas_sampler: sampler;

struct GlyphOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> GlyphOut {
    var out: GlyphOut;
    out.pos = vec4<f32>(position, 0.0, 1.0);
    out.uv = uv;
    out.color = color;
    return out;
}

@fragment
fn fs_main(
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
) -> @location(0) vec4<f32> {
    let coverage: f32 = textureSample(atlas_tex, atlas_sampler, uv).r;
    if (coverage < 0.004) {
        discard;
    }
    return vec4<f32>(color.rgb, color.a * coverage);
}
"#;

/// Long-lived GPU objects for one surface, reused across frames.
pub(crate) struct GpuResources {
    format: TextureFormat,
    atlas_dims: AtlasDims,
    fill_pipeline: RenderPipeline,
    glyph_pipeline: RenderPipeline,
    fill_vb: Buffer,
    glyph_vb: Buffer,
    index_buf: Buffer,
    index_capacity_quads: usize,
    atlas_tex: Texture,
    atlas_bind: BindGroup,
    inline_tex: Texture,
    inline_bind: BindGroup,
    last_atlas_texels: Option<Vec<u8>>,
    last_atlas_dims: Option<AtlasDims>,
}

impl std::fmt::Debug for GpuResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuResources")
            .field("format", &self.format)
            .field("atlas_dims", &self.atlas_dims)
            .field("index_capacity_quads", &self.index_capacity_quads)
            .field("has_last_atlas", &self.last_atlas_texels.is_some())
            .finish_non_exhaustive()
    }
}

fn fill_buffer_size() -> u64 {
    (MAX_FILL_QUADS_PER_BATCH * VERTICES_PER_QUAD * FILL_VERTEX_SIZE_BYTES) as u64
}

fn glyph_buffer_size() -> u64 {
    (MAX_GLYPH_QUADS_PER_BATCH * VERTICES_PER_QUAD * GLYPH_VERTEX_SIZE_BYTES) as u64
}

fn index_buffer_size() -> u64 {
    (MAX_FILL_QUADS_PER_BATCH.max(MAX_GLYPH_QUADS_PER_BATCH) * INDICES_PER_QUAD * 2) as u64
}

fn make_sampler(device: &wgpu::Device) -> Sampler {
    device.create_sampler(&SamplerDescriptor {
        label: Some("bitty-atlas-sampler"),
        address_mode_u: AddressMode::ClampToEdge,
        address_mode_v: AddressMode::ClampToEdge,
        address_mode_w: AddressMode::ClampToEdge,
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        mipmap_filter: FilterMode::Nearest,
        lod_min_clamp: 0.0,
        lod_max_clamp: 1.0,
        compare: None,
        anisotropy_clamp: 1,
        border_color: None,
    })
}

fn make_bind_layout(device: &wgpu::Device) -> BindGroupLayout {
    use wgpu::{BindingType, TextureViewDimension};
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("bitty-glyph-bind-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: true },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &BindGroupLayout,
    view: &TextureView,
    sampler: &Sampler,
    label: &'static str,
) -> BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn make_r8_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &'static str,
) -> Result<Texture, RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidInput {
            reason: "texture dimensions must be non-zero",
        });
    }
    Ok(device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::R8Unorm,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    }))
}

impl GpuResources {
    /// Creates all GPU objects for `format` + `atlas_dims`.
    pub(crate) fn create(
        device: &wgpu::Device,
        format: TextureFormat,
        atlas_dims: AtlasDims,
    ) -> Result<Self, RenderError> {
        batch::validate_atlas_dims(atlas_dims).map_err(|_| {
            RenderError::UpstreamGraphics("atlas dimensions exceed the GPU upload cap".into())
        })?;

        let fill_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("bitty-fill-shader"),
            source: ShaderSource::Wgsl(FILL_WGSL.into()),
        });
        let glyph_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("bitty-glyph-shader"),
            source: ShaderSource::Wgsl(GLYPH_WGSL.into()),
        });

        let bind_layout = make_bind_layout(device);
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("bitty-present-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        // The fill pipeline carries no texture bindings, but sharing the
        // layout keeps bind-group switching free between passes.
        let empty_pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("bitty-fill-layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let fill_attributes = [
            VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: VertexFormat::Float32x2,
            },
            VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: VertexFormat::Float32x4,
            },
        ];
        let fill_layout = VertexBufferLayout {
            array_stride: FILL_VERTEX_SIZE_BYTES as u64,
            step_mode: VertexStepMode::Vertex,
            attributes: &fill_attributes,
        };
        let fill_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("bitty-fill-pipeline"),
            layout: Some(&empty_pipeline_layout),
            vertex: VertexState {
                module: &fill_module,
                entry_point: Some("vs_main"),
                buffers: &[fill_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(FragmentState {
                module: &fill_module,
                entry_point: Some("fs_main"),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        let glyph_attributes = [
            VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: VertexFormat::Float32x2,
            },
            VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: VertexFormat::Float32x2,
            },
            VertexAttribute {
                offset: 16,
                shader_location: 2,
                format: VertexFormat::Float32x4,
            },
        ];
        let glyph_layout = VertexBufferLayout {
            array_stride: GLYPH_VERTEX_SIZE_BYTES as u64,
            step_mode: VertexStepMode::Vertex,
            attributes: &glyph_attributes,
        };
        let glyph_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("bitty-glyph-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &glyph_module,
                entry_point: Some("vs_main"),
                buffers: &[glyph_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(FragmentState {
                module: &glyph_module,
                entry_point: Some("fs_main"),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        let fill_vb = device.create_buffer(&BufferDescriptor {
            label: Some("bitty-fill-vb"),
            size: fill_buffer_size(),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_vb = device.create_buffer(&BufferDescriptor {
            label: Some("bitty-glyph-vb"),
            size: glyph_buffer_size(),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let index_buf = device.create_buffer(&BufferDescriptor {
            label: Some("bitty-quad-ib"),
            size: index_buffer_size(),
            usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let index_capacity_quads = MAX_FILL_QUADS_PER_BATCH.max(MAX_GLYPH_QUADS_PER_BATCH);
        // Validate the bounded index build up front (fail-closed); the bytes
        // are uploaded per frame via `ensure_indices` once a queue is in use.
        batch::quad_indices_for(index_capacity_quads).ok_or_else(|| {
            RenderError::UpstreamGraphics("quad index build exceeded the bounded cap".into())
        })?;

        let atlas_tex = make_r8_texture(
            device,
            u32::from(atlas_dims.width),
            u32::from(atlas_dims.height),
            "bitty-atlas",
        )?;
        let atlas_view = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = make_sampler(device);
        let atlas_bind = make_bind_group(
            device,
            &bind_layout,
            &atlas_view,
            &sampler,
            "bitty-atlas-bind",
        );

        let inline_side = batch::INLINE_TEXTURE_SIZE;
        let inline_tex = make_r8_texture(device, inline_side, inline_side, "bitty-inline")?;
        let inline_view = inline_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let inline_bind = make_bind_group(
            device,
            &bind_layout,
            &inline_view,
            &sampler,
            "bitty-inline-bind",
        );

        Ok(Self {
            format,
            atlas_dims,
            fill_pipeline,
            glyph_pipeline,
            fill_vb,
            glyph_vb,
            index_buf,
            index_capacity_quads,
            atlas_tex,
            atlas_bind,
            inline_tex,
            inline_bind,
            last_atlas_texels: None,
            last_atlas_dims: None,
        })
    }

    /// True when these resources match the surface format and atlas size.
    pub(crate) fn matches(&self, format: TextureFormat, dims: AtlasDims) -> bool {
        self.format == format && self.atlas_dims == dims
    }

    /// The surface format these resources were built for.
    pub(crate) fn format_for_match(&self) -> TextureFormat {
        self.format
    }

    /// The atlas dimensions these resources were built for.
    pub(crate) fn atlas_dims_for_match(&self) -> AtlasDims {
        self.atlas_dims
    }

    /// Uploads the shared quad index buffer (idempotent, bounded).
    fn ensure_indices(&self, queue: &wgpu::Queue, quad_count: usize) -> Result<(), RenderError> {
        if quad_count > self.index_capacity_quads {
            return Err(RenderError::InvalidInput {
                reason: "draw batch exceeds the bounded index cap",
            });
        }
        let indices = batch::quad_indices_for(self.index_capacity_quads).ok_or(
            RenderError::InvalidInput {
                reason: "draw batch exceeds the bounded index cap",
            },
        )?;
        let bytes = batch::indices_to_le_bytes(&indices);
        if bytes.len() as u64 > index_buffer_size() {
            return Err(RenderError::InvalidInput {
                reason: "draw batch exceeds the bounded index cap",
            });
        }
        queue.write_buffer(&self.index_buf, 0, &bytes);
        Ok(())
    }

    /// Uploads atlas texels with dirty-region invalidation.
    fn upload_atlas(
        &mut self,
        queue: &wgpu::Queue,
        texels: &[u8],
        dims: AtlasDims,
    ) -> Result<(), RenderError> {
        batch::validate_atlas_dims(dims)?;
        let expected = dims.width as usize * dims.height as usize;
        if texels.len() != expected {
            return Err(RenderError::InvalidInput {
                reason: "atlas texel length does not match its dimensions",
            });
        }
        let prev = match (&self.last_atlas_texels, self.last_atlas_dims) {
            (Some(prev), Some(prev_dims)) => Some((prev.as_slice(), prev_dims)),
            _ => None,
        };
        match batch::compute_atlas_dirty(prev, texels, dims) {
            AtlasDirty::Clean => Ok(()),
            AtlasDirty::Full => {
                let padded =
                    batch::build_padded_full(texels, dims).ok_or(RenderError::InvalidInput {
                        reason: "atlas texel length does not match its dimensions",
                    })?;
                let bytes_per_row = batch::padded_bytes_per_row(u32::from(dims.width));
                let size = Extent3d {
                    width: u32::from(dims.width),
                    height: u32::from(dims.height),
                    depth_or_array_layers: 1,
                };
                if padded.len() as u64 > u64::from(bytes_per_row) * u64::from(dims.height) {
                    return Err(RenderError::InvalidInput {
                        reason: "atlas upload exceeds the bounded staging cap",
                    });
                }
                queue.write_texture(
                    TexelCopyTextureInfo {
                        texture: &self.atlas_tex,
                        mip_level: 0,
                        origin: Origin3d::ZERO,
                        aspect: TextureAspect::All,
                    },
                    &padded,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bytes_per_row),
                        rows_per_image: Some(u32::from(dims.height)),
                    },
                    size,
                );
                self.last_atlas_texels = Some(texels.to_vec());
                self.last_atlas_dims = Some(dims);
                Ok(())
            }
            AtlasDirty::Strip { y, height } => {
                let padded = batch::build_padded_strip(texels, dims, y, height).ok_or(
                    RenderError::InvalidInput {
                        reason: "atlas texel length does not match its dimensions",
                    },
                )?;
                let bytes_per_row = batch::padded_bytes_per_row(u32::from(dims.width));
                queue.write_texture(
                    TexelCopyTextureInfo {
                        texture: &self.atlas_tex,
                        mip_level: 0,
                        origin: Origin3d { x: 0, y, z: 0 },
                        aspect: TextureAspect::All,
                    },
                    &padded,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bytes_per_row),
                        rows_per_image: Some(height),
                    },
                    Extent3d {
                        width: u32::from(dims.width),
                        height,
                        depth_or_array_layers: 1,
                    },
                );
                self.last_atlas_texels = Some(texels.to_vec());
                self.last_atlas_dims = Some(dims);
                Ok(())
            }
        }
    }

    /// Uploads the transient inline texture for this frame.
    fn upload_inline(
        &self,
        queue: &wgpu::Queue,
        plan: &batch::InlinePlan,
    ) -> Result<(), RenderError> {
        if plan.placements.is_empty() {
            return Ok(());
        }
        let expected = plan.width as usize * plan.height as usize;
        if plan.texels.len() != expected
            || plan.width != batch::INLINE_TEXTURE_SIZE
            || plan.height != batch::INLINE_TEXTURE_SIZE
        {
            return Err(RenderError::InvalidInput {
                reason: "inline texture length does not match its dimensions",
            });
        }
        let padded = batch::build_padded_full(
            &plan.texels,
            AtlasDims {
                width: plan.width as u16,
                height: plan.height as u16,
            },
        )
        .ok_or(RenderError::InvalidInput {
            reason: "inline texture length does not match its dimensions",
        })?;
        let bytes_per_row = batch::padded_bytes_per_row(plan.width);
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &self.inline_tex,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            &padded,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(plan.height),
            },
            Extent3d {
                width: plan.width,
                height: plan.height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }

    /// Draws one frame's batches inside a single render pass.
    ///
    /// `clear` is the load-operation clear color (the surface background).
    /// All buffer writes are bounds-checked before submission; overruns
    /// return [`RenderError::InvalidInput`] without touching the GPU.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &TextureView,
        surface_w: u32,
        surface_h: u32,
        scale: f32,
        draw_list: &DrawList,
        atlas: Option<(&[u8], AtlasDims)>,
        clear: wgpu::Color,
    ) -> Result<(), RenderError> {
        if surface_w == 0 || surface_h == 0 {
            return Err(RenderError::InvalidInput {
                reason: "surface extent must be non-zero",
            });
        }
        // Atlas upload first so sampling sees fresh texels.
        if let Some((texels, dims)) = atlas {
            if dims != self.atlas_dims {
                return Err(RenderError::InvalidInput {
                    reason: "atlas dimensions changed without resource recreation",
                });
            }
            self.upload_atlas(queue, texels, dims)?;
        }
        let needs_atlas = draw_list
            .glyphs
            .iter()
            .any(|g| matches!(g.source, crate::grid::GlyphSource::Atlas { .. }));
        if needs_atlas && atlas.is_none() {
            return Err(RenderError::InvalidInput {
                reason: "atlas instance requires atlas texels",
            });
        }

        let fill_chunks = batch::chunk_fills(&draw_list.fills, surface_w, surface_h, scale);
        let atlas_chunks =
            batch::chunk_atlas_glyphs(&draw_list.glyphs, surface_w, surface_h, scale);
        let inline_plan = batch::pack_inline_glyphs(&draw_list.glyphs);
        self.upload_inline(queue, &inline_plan)?;
        let inline_chunks = batch::chunk_inline_glyphs(&inline_plan, surface_w, surface_h, scale);

        // Index buffer covers the largest single chunk this frame.
        let max_quads = fill_chunks
            .iter()
            .map(|c| c.quad_count)
            .chain(atlas_chunks.iter().map(|c| c.quad_count))
            .chain(inline_chunks.iter().map(|c| c.quad_count))
            .max()
            .unwrap_or(0);
        if max_quads > 0 {
            self.ensure_indices(queue, max_quads)?;
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("bitty-present-draw-list"),
        });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("bitty-draw-list"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            for chunk in &fill_chunks {
                if chunk.bytes.len() as u64 > fill_buffer_size() {
                    return Err(RenderError::InvalidInput {
                        reason: "draw batch exceeds the bounded vertex cap",
                    });
                }
                queue.write_buffer(&self.fill_vb, 0, &chunk.bytes);
                pass.set_pipeline(&self.fill_pipeline);
                pass.set_vertex_buffer(0, self.fill_vb.slice(..chunk.bytes.len() as u64));
                pass.set_index_buffer(
                    self.index_buf
                        .slice(..(chunk.quad_count * INDICES_PER_QUAD * 2) as u64),
                    IndexFormat::Uint16,
                );
                pass.draw_indexed(0..(chunk.quad_count * INDICES_PER_QUAD) as u32, 0, 0..1);
            }

            for chunk in &atlas_chunks {
                if chunk.bytes.len() as u64 > glyph_buffer_size() {
                    return Err(RenderError::InvalidInput {
                        reason: "draw batch exceeds the bounded vertex cap",
                    });
                }
                queue.write_buffer(&self.glyph_vb, 0, &chunk.bytes);
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.atlas_bind, &[]);
                pass.set_vertex_buffer(0, self.glyph_vb.slice(..chunk.bytes.len() as u64));
                pass.set_index_buffer(
                    self.index_buf
                        .slice(..(chunk.quad_count * INDICES_PER_QUAD * 2) as u64),
                    IndexFormat::Uint16,
                );
                pass.draw_indexed(0..(chunk.quad_count * INDICES_PER_QUAD) as u32, 0, 0..1);
            }

            for chunk in &inline_chunks {
                if chunk.bytes.len() as u64 > glyph_buffer_size() {
                    return Err(RenderError::InvalidInput {
                        reason: "draw batch exceeds the bounded vertex cap",
                    });
                }
                queue.write_buffer(&self.glyph_vb, 0, &chunk.bytes);
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.inline_bind, &[]);
                pass.set_vertex_buffer(0, self.glyph_vb.slice(..chunk.bytes.len() as u64));
                pass.set_index_buffer(
                    self.index_buf
                        .slice(..(chunk.quad_count * INDICES_PER_QUAD * 2) as u64),
                    IndexFormat::Uint16,
                );
                pass.draw_indexed(0..(chunk.quad_count * INDICES_PER_QUAD) as u32, 0, 0..1);
            }
        }
        queue.submit(std::iter::once(encoder.finish()));
        Ok(())
    }
}

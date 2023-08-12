use super::{
    context::{FrameArenas, GraphicsContext},
    draw::{DrawParam, DrawUniforms},
    gpu::{
        arc::{ArcBindGroup, ArcBindGroupLayout, ArcBuffer, ArcShaderModule, ArcTextureView},
        bind_group::{BindGroupBuilder, BindGroupCache, BindGroupLayoutBuilder},
        growing::{ArenaAllocation, GrowingBufferArena},
        pipeline::{PipelineCache, RenderPipelineInfo},
        text::{TextRenderer, TextVertex},
    },
    image::Image,
    mesh::{Mesh, Vertex},
    sampler::{Sampler, SamplerCache},
    shader::Shader,
    BlendMode, Color, InstanceArray, LinearColor, Rect, Text, Transform, WgpuContext,
};
use crate::{GameError, GameResult};
use crevice::std140::AsStd140;
use std::{collections::HashMap, hash::Hash};

/// A canvas represents a render pass and is how you render primitives such as meshes and text onto images.
#[allow(missing_debug_implementations)]
pub struct InternalCanvas<'a> {
    wgpu: &'a WgpuContext,
    arenas: &'a FrameArenas,
    bind_group_cache: &'a mut BindGroupCache,
    pipeline_cache: &'a mut PipelineCache,
    sampler_cache: &'a mut SamplerCache,
    text_renderer: &'a mut TextRenderer,
    fonts: &'a HashMap<String, glyph_brush::FontId>,
    uniform_arena: &'a mut GrowingBufferArena,

    shader: Shader,
    shader_bind_group: Option<(&'a wgpu::BindGroup, ArcBindGroupLayout, u32)>,
    text_shader: Shader,
    text_shader_bind_group: Option<(&'a wgpu::BindGroup, ArcBindGroupLayout, u32)>,

    shader_ty: Option<ShaderType>,
    dirty_pipeline: bool,
    queuing_text: bool,
    blend_mode: BlendMode,
    pass: wgpu::RenderPass<'a>,
    samples: u32,
    format: wgpu::TextureFormat,
    text_uniforms: ArenaAllocation,

    draw_sm: ArcShaderModule,
    instance_sm: ArcShaderModule,
    instance_unordered_sm: ArcShaderModule,
    text_sm: ArcShaderModule,

    transform: glam::Mat4,
    curr_image: Option<ArcTextureView>,
    curr_sampler: Sampler,
    next_sampler: Sampler,
    premul_text: bool,
}

impl<'a> InternalCanvas<'a> {
    pub fn from_image(
        gfx: &'a mut GraphicsContext,
        clear: impl Into<Option<Color>>,
        image: &'a Image,
    ) -> GameResult<Self> {
        if image.samples() > 1 {
            return Err(GameError::RenderError(String::from("non-MSAA rendering requires an image with exactly 1 sample, for this image use Canvas::from_msaa instead")));
        }

        Self::new(gfx, 1, image.format(), |cmd| {
            cmd.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: image.view.as_ref(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: match clear.into() {
                            None => wgpu::LoadOp::Load,
                            Some(color) => wgpu::LoadOp::Clear(LinearColor::from(color).into()),
                        },
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
            })
        })
    }

    pub fn from_msaa(
        gfx: &'a mut GraphicsContext,
        clear: impl Into<Option<Color>>,
        msaa_image: &'a Image,
        resolve_image: &'a Image,
    ) -> GameResult<Self> {
        if msaa_image.samples() == 1 {
            return Err(GameError::RenderError(String::from(
                "MSAA rendering requires an image with more than 1 sample, for this image use Canvas::from_image instead",
            )));
        }

        if resolve_image.samples() > 1 {
            return Err(GameError::RenderError(String::from(
                "can only resolve into an image with exactly 1 sample",
            )));
        }

        if msaa_image.format() != resolve_image.format() {
            return Err(GameError::RenderError(String::from(
                "MSAA image and resolve image must be the same format",
            )));
        }

        Self::new(gfx, msaa_image.samples(), msaa_image.format(), |cmd| {
            cmd.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: msaa_image.view.as_ref(),
                    resolve_target: Some(resolve_image.view.as_ref()),
                    ops: wgpu::Operations {
                        load: match clear.into() {
                            None => wgpu::LoadOp::Load,
                            Some(color) => wgpu::LoadOp::Clear(LinearColor::from(color).into()),
                        },
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
            })
        })
    }

    pub(crate) fn new(
        gfx: &'a mut GraphicsContext,
        samples: u32,
        format: wgpu::TextureFormat,
        create_pass: impl FnOnce(&'a mut wgpu::CommandEncoder) -> wgpu::RenderPass<'a>,
    ) -> GameResult<Self> {
        if gfx.fcx.is_none() {
            return Err(GameError::RenderError(String::from(
                "starting Canvas outside of a frame",
            )));
        }

        let drawable_size = gfx.drawable_size();

        let wgpu = &gfx.wgpu;
        let bind_group_cache = &mut gfx.bind_group_cache;
        let pipeline_cache = &mut gfx.pipeline_cache;
        let sampler_cache = &mut gfx.sampler_cache;
        let text_renderer = &mut gfx.text;
        let fonts = &gfx.fonts;
        let uniform_arena = &mut gfx.uniform_arena;

        let (arenas, mut pass) = {
            let fcx = gfx.fcx.as_mut().unwrap(/* see above */);

            let pass = create_pass(&mut fcx.cmd);
            let arenas = &fcx.arenas;

            (arenas, pass)
        };

        pass.set_blend_constant(wgpu::Color::BLACK);

        let screen_coords = Rect {
            x: 0.,
            y: 0.,
            w: drawable_size.0 as _,
            h: drawable_size.1 as _,
        };
        let transform = screen_to_mat(screen_coords);

        let shader = Shader {
            vs_module: None,
            fs_module: None,
        };

        let text_shader = Shader {
            vs_module: None,
            fs_module: None,
        };

        let text_uniforms =
            uniform_arena.allocate(&wgpu.device, TextUniforms::std140_size_static() as _);

        wgpu.queue.write_buffer(
            &text_uniforms.buffer,
            text_uniforms.offset,
            (TextUniforms {
                transform: transform.into(),
            })
            .as_std140()
            .as_bytes(),
        );

        Ok(InternalCanvas {
            wgpu,
            arenas,
            bind_group_cache,
            pipeline_cache,
            sampler_cache,
            text_renderer,
            fonts,
            uniform_arena,

            shader,
            shader_bind_group: None,
            text_shader,
            text_shader_bind_group: None,

            shader_ty: None,
            dirty_pipeline: true,
            queuing_text: false,
            blend_mode: BlendMode::ALPHA,
            pass,
            samples,
            format,
            text_uniforms,

            draw_sm: gfx.draw_shader.clone(),
            instance_sm: gfx.instance_shader.clone(),
            instance_unordered_sm: gfx.instance_unordered_shader.clone(),
            text_sm: gfx.text_shader.clone(),

            transform,
            curr_image: None,
            curr_sampler: Sampler::default(),
            next_sampler: Sampler::default(),
            premul_text: true,
        })
    }

    pub fn set_shader_params(
        &mut self,
        bind_group: ArcBindGroup,
        layout: ArcBindGroupLayout,
        offset: u32,
    ) {
        self.flush_text();
        self.dirty_pipeline = true;
        self.shader_bind_group = Some((self.arenas.bind_groups.alloc(bind_group), layout, offset));
    }

    pub fn set_shader(&mut self, shader: Shader) {
        self.flush_text();
        self.dirty_pipeline = true;
        self.shader = shader;
    }

    pub fn set_text_shader_params(
        &mut self,
        bind_group: ArcBindGroup,
        layout: ArcBindGroupLayout,
        offset: u32,
    ) {
        self.flush_text();
        self.dirty_pipeline = true;
        self.text_shader_bind_group =
            Some((self.arenas.bind_groups.alloc(bind_group), layout, offset));
    }

    pub fn set_text_shader(&mut self, shader: Shader) {
        self.flush_text();
        self.dirty_pipeline = true;
        self.text_shader = shader;
    }

    pub fn set_sampler(&mut self, sampler: Sampler) {
        self.flush_text();
        self.next_sampler = sampler;
    }

    pub fn set_blend_mode(&mut self, blend_mode: BlendMode) {
        self.flush_text();
        self.dirty_pipeline = true;
        self.blend_mode = blend_mode;
    }

    pub fn set_premultiplied_text(&mut self, premultiplied_text: bool) {
        self.flush_text();
        self.premul_text = premultiplied_text;
    }

    pub fn set_projection(&mut self, proj: impl Into<mint::ColumnMatrix4<f32>>) {
        self.flush_text();
        self.transform = proj.into().into();
        self.text_uniforms = self
            .uniform_arena
            .allocate(&self.wgpu.device, TextUniforms::std140_size_static() as _);
        self.wgpu.queue.write_buffer(
            &self.text_uniforms.buffer,
            self.text_uniforms.offset,
            (TextUniforms {
                transform: self.transform.into(),
            })
            .as_std140()
            .as_bytes(),
        );
    }

    pub fn set_scissor_rect(&mut self, (x, y, w, h): (u32, u32, u32, u32)) {
        self.flush_text();
        self.pass.set_scissor_rect(x, y, w, h);
    }

    #[allow(unsafe_code)]
    pub fn draw_mesh(&mut self, mesh: &'a Mesh, image: &Image, param: DrawParam, scale: bool) {
        self.flush_text();
        self.update_pipeline(ShaderType::Draw);

        let alloc_size = DrawUniforms::std140_size_static() as u64;
        let uniform_alloc = self.uniform_arena.allocate(&self.wgpu.device, alloc_size);

        let (uniform_bind_group, _) = BindGroupBuilder::new()
            .buffer(
                &uniform_alloc.buffer,
                0,
                wgpu::ShaderStages::VERTEX,
                wgpu::BufferBindingType::Uniform,
                true,
                Some(alloc_size),
            )
            .create(&self.wgpu.device, self.bind_group_cache);

        self.set_image(image.clone());

        let mut uniforms = DrawUniforms::from_param(
            &param,
            if scale {
                Some(glam::Vec2::new(image.width() as f32, image.height() as f32).into())
            } else {
                None
            },
        );
        uniforms.transform = (self.transform * glam::Mat4::from(uniforms.transform)).into();

        // 1. allocate some uniform buffer memory from GrowingBufferArena.
        // 2. write the uniform data to that memory
        // 3. use a "dynamic offset" to offset into the memory

        self.wgpu.queue.write_buffer(
            &uniform_alloc.buffer,
            uniform_alloc.offset,
            uniforms.as_std140().as_bytes(),
        );

        self.pass.set_bind_group(
            0,
            self.arenas.bind_groups.alloc(uniform_bind_group),
            &[uniform_alloc.offset as u32], // <- the dynamic offset
        );

        self.pass.set_vertex_buffer(0, mesh.verts.slice(..));
        self.pass
            .set_index_buffer(mesh.inds.slice(..), wgpu::IndexFormat::Uint32);

        self.pass.draw_indexed(0..mesh.index_count as _, 0, 0..1);
    }

    pub fn draw_mesh_instances(
        &mut self,
        mesh: &'a Mesh,
        instances: &'a InstanceArrayView,
        param: DrawParam,
        scale: bool,
    ) -> GameResult {
        self.flush_text();

        if instances.len == 0 {
            return Ok(());
        }

        self.update_pipeline(ShaderType::Instance {
            ordered: instances.ordered,
        });

        let alloc_size = u64::from(
            self.wgpu
                .device
                .limits()
                .min_uniform_buffer_offset_alignment,
        );
        let uniform_alloc = self.uniform_arena.allocate(&self.wgpu.device, alloc_size);

        let (uniform_bind_group, _) = BindGroupBuilder::new()
            .buffer(
                &uniform_alloc.buffer,
                0,
                wgpu::ShaderStages::VERTEX,
                wgpu::BufferBindingType::Uniform,
                true,
                Some(alloc_size),
            )
            .create(&self.wgpu.device, self.bind_group_cache);

        self.set_image(instances.image.clone());

        let uniforms = InstanceUniforms {
            transform: (self.transform
                * glam::Mat4::from(
                    // image scaling is non-sensical for instance array itself as the image scaling is applied locally (see below)
                    DrawUniforms::from_param(&param, None).transform,
                ))
            .into(),
            color: mint::Vector4::<f32> {
                x: param.color.r,
                y: param.color.g,
                z: param.color.b,
                w: param.color.a,
            },
            // this is the actual image scale that we apply in the vertex shader.
            // we can't apply this when we first convert the instance array drawparams because we don't know the image size at the time the user inserts the drawparams.
            // we also can't apply image scaling in the global instance transform as it *must* be applied in local space.
            scale: if scale {
                glam::Vec2::new(
                    instances.image.width() as f32,
                    instances.image.height() as f32,
                )
            } else {
                glam::Vec2::ZERO
            }
            .into(),
        };

        self.wgpu.queue.write_buffer(
            &uniform_alloc.buffer,
            uniform_alloc.offset,
            uniforms.as_std140().as_bytes(),
        );

        self.pass.set_bind_group(
            0,
            self.arenas.bind_groups.alloc(uniform_bind_group),
            &[uniform_alloc.offset as u32],
        );
        self.pass.set_bind_group(2, &instances.bind_group, &[]);

        self.pass.set_vertex_buffer(0, mesh.verts.slice(..));
        self.pass
            .set_index_buffer(mesh.inds.slice(..), wgpu::IndexFormat::Uint32);

        self.pass
            .draw_indexed(0..mesh.index_count as _, 0, 0..instances.len as _);

        Ok(())
    }

    pub fn draw_bounded_text(&mut self, text: &Text, mut param: DrawParam) -> GameResult {
        if let Transform::Values { dest, offset, .. } = &mut param.transform {
            if offset.x > 0. || offset.y > 0. {
                let bounds = text.measure_raw(self.text_renderer, self.fonts)?;
                dest.x -= offset.x * bounds.x;
                dest.y -= offset.y * bounds.y;
                *offset = mint::Point2 { x: 0., y: 0. };
            }
        }

        self.text_renderer
            .queue(text.as_section(self.fonts, param)?);

        self.set_text_image(self.text_renderer.cache_view.clone());

        let (text_uniforms_bind, _) = BindGroupBuilder::new()
            .buffer(
                &self.text_uniforms.buffer,
                0,
                wgpu::ShaderStages::VERTEX,
                wgpu::BufferBindingType::Uniform,
                true,
                Some(TextUniforms::std140_size_static() as _),
            )
            .create(&self.wgpu.device, self.bind_group_cache);

        self.pass.set_bind_group(
            0,
            self.arenas.bind_groups.alloc(text_uniforms_bind),
            &[self.text_uniforms.offset as u32],
        );

        self.queuing_text = true;

        Ok(())
    }

    fn flush_text(&mut self) {
        if self.queuing_text {
            self.queuing_text = false;
            let mut premul = false;
            if self.premul_text && self.blend_mode == BlendMode::ALPHA {
                premul = true;
                self.set_blend_mode(BlendMode::PREMULTIPLIED);
            }
            self.update_pipeline(ShaderType::Text);
            self.text_renderer.draw_queued(
                &self.wgpu.device,
                &self.wgpu.queue,
                self.arenas,
                &mut self.pass,
            );
            if premul {
                self.set_blend_mode(BlendMode::ALPHA);
            }
        }
    }

    pub fn finish(mut self) {
        self.finalize();
    }

    fn finalize(&mut self) {
        self.flush_text();
    }

    fn update_pipeline(&mut self, ty: ShaderType) {
        if self.dirty_pipeline || self.shader_ty != Some(ty) {
            self.dirty_pipeline = false;
            self.shader_ty = Some(ty);

            let texture_layout = BindGroupLayoutBuilder::new()
                .image(wgpu::ShaderStages::FRAGMENT)
                .sampler(wgpu::ShaderStages::FRAGMENT)
                .create(&self.wgpu.device, self.bind_group_cache);

            let instance_layout = BindGroupLayoutBuilder::new()
                .buffer(
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BufferBindingType::Uniform,
                    false,
                )
                .buffer(
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BufferBindingType::Uniform,
                    false,
                )
                .create(&self.wgpu.device, self.bind_group_cache);

            let uniform_layout = BindGroupLayoutBuilder::new()
                .seed(ty)
                .buffer(
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BufferBindingType::Uniform,
                    true,
                )
                .create(&self.wgpu.device, self.bind_group_cache);

            let (dummy_group, dummy_layout) =
                BindGroupBuilder::new().create(&self.wgpu.device, self.bind_group_cache);

            let mut groups = vec![uniform_layout, texture_layout];

            if let ShaderType::Instance { .. } = ty {
                groups.push(instance_layout);
            } else {
                // the dummy group ensures the user's bind group is at index 3
                groups.push(dummy_layout);
                self.pass
                    .set_bind_group(2, self.arenas.bind_groups.alloc(dummy_group), &[]);
            }

            let shader = match ty {
                ShaderType::Draw | ShaderType::Instance { .. } => {
                    if let Some((bind_group, bind_group_layout, offset)) = &self.shader_bind_group {
                        self.pass.set_bind_group(3, bind_group, &[*offset]);
                        groups.push(bind_group_layout.clone());
                    }

                    &self.shader
                }
                ShaderType::Text => {
                    if let Some((bind_group, bind_group_layout, offset)) =
                        &self.text_shader_bind_group
                    {
                        self.pass.set_bind_group(3, bind_group, &[*offset]);
                        groups.push(bind_group_layout.clone());
                    }

                    &self.text_shader
                }
            };

            let layout = self.pipeline_cache.layout(&self.wgpu.device, &groups);
            let pipeline = self
                .arenas
                .render_pipelines
                .alloc(self.pipeline_cache.render_pipeline(
                    &self.wgpu.device,
                    layout.as_ref(),
                    RenderPipelineInfo {
                        vs: if let Some(vs_module) = &shader.vs_module {
                            vs_module.clone()
                        } else {
                            match ty {
                                ShaderType::Draw => self.draw_sm.clone(),
                                ShaderType::Instance { ordered } => {
                                    if ordered {
                                        self.instance_sm.clone()
                                    } else {
                                        self.instance_unordered_sm.clone()
                                    }
                                }
                                ShaderType::Text => self.text_sm.clone(),
                            }
                        },
                        fs: if let Some(fs_module) = &shader.fs_module {
                            fs_module.clone()
                        } else {
                            match ty {
                                ShaderType::Draw | ShaderType::Instance { .. } => {
                                    self.draw_sm.clone()
                                }
                                ShaderType::Text => self.text_sm.clone(),
                            }
                        },
                        vs_entry: "vs_main".into(),
                        fs_entry: "fs_main".into(),
                        samples: self.samples,
                        format: self.format,
                        blend: Some(wgpu::BlendState {
                            color: self.blend_mode.color,
                            alpha: self.blend_mode.alpha,
                        }),
                        depth: false,
                        vertices: true,
                        topology: match ty {
                            ShaderType::Text => wgpu::PrimitiveTopology::TriangleStrip,
                            _ => wgpu::PrimitiveTopology::TriangleList,
                        },
                        vertex_layout: match ty {
                            ShaderType::Text => TextVertex::layout(),
                            _ => Vertex::layout(),
                        },
                    },
                ));

            self.pass.set_pipeline(pipeline);
        }
    }

    fn set_image(&mut self, image: Image) {
        if self.curr_sampler != self.next_sampler
            || self
                .curr_image
                .as_ref()
                .map_or(true, |curr| curr.id() != image.view.id())
        {
            self.curr_sampler = self.next_sampler;
            let sample = self.sampler_cache.get(&self.wgpu.device, self.curr_sampler);
            let image_bind = image.fetch_buffer(sample.id(), sample, &self.wgpu.device);

            self.curr_image = Some(image.view);

            self.pass
                .set_bind_group(1, self.arenas.bind_groups.alloc(image_bind), &[]);
        }
    }

    fn set_text_image(&mut self, view: ArcTextureView) {
        if self.curr_sampler != self.next_sampler
            || self
                .curr_image
                .as_ref()
                .map_or(true, |curr| curr.id() != view.id())
        {
            self.curr_sampler = self.next_sampler;

            let (image_bind, _) = BindGroupBuilder::new()
                .image(&view, wgpu::ShaderStages::FRAGMENT)
                .sampler(
                    &self.sampler_cache.get(&self.wgpu.device, self.curr_sampler),
                    wgpu::ShaderStages::FRAGMENT,
                )
                .create(&self.wgpu.device, self.bind_group_cache);

            self.curr_image = Some(view);

            self.pass
                .set_bind_group(1, self.arenas.bind_groups.alloc(image_bind), &[]);
        }
    }
}

impl<'a> Drop for InternalCanvas<'a> {
    fn drop(&mut self) {
        self.finalize();
    }
}

#[derive(Debug)]
pub struct InstanceArrayView {
    pub buffer: ArcBuffer,
    pub indices: ArcBuffer,
    pub bind_group: ArcBindGroup,
    pub image: Image,
    pub len: u32,
    pub ordered: bool,
}

impl InstanceArrayView {
    pub fn from_instances(ia: &InstanceArray) -> GameResult<Self> {
        Ok(InstanceArrayView {
            buffer: ia.buffer.lock().map_err(|_| GameError::LockError)?.clone(),
            indices: ia.indices.lock().map_err(|_| GameError::LockError)?.clone(),
            bind_group: ia
                .bind_group
                .lock()
                .map_err(|_| GameError::LockError)?
                .clone(),
            image: ia.image.clone(),
            len: ia.instances().len() as u32,
            ordered: ia.ordered,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ShaderType {
    Draw,
    Instance { ordered: bool },
    Text,
}

#[derive(crevice::std140::AsStd140)]
struct InstanceUniforms {
    pub transform: mint::ColumnMatrix4<f32>,
    pub color: mint::Vector4<f32>,
    pub scale: mint::Vector2<f32>,
}

#[derive(crevice::std140::AsStd140)]
struct TextUniforms {
    transform: mint::ColumnMatrix4<f32>,
}

pub(crate) fn screen_to_mat(screen: Rect) -> glam::Mat4 {
    glam::Mat4::orthographic_rh(
        screen.left(),
        screen.right(),
        screen.bottom(),
        screen.top(),
        0.,
        1.,
    )
}

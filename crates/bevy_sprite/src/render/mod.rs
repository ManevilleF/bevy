use std::cmp::Ordering;
use std::ops::Range;

use crate::{
    texture_atlas::{TextureAtlas, TextureAtlasSprite},
    Rect, Sprite, SPRITE_SHADER_HANDLE,
};
use bevy_asset::{AssetEvent, Assets, Handle, HandleId};
use bevy_core_pipeline::core_2d::Transparent2d;
use bevy_ecs::{
    prelude::*,
    system::{lifetimeless::*, SystemParamItem},
};
use bevy_math::Vec2;
use bevy_reflect::Uuid;
use bevy_render::{
    color::Color,
    render_asset::RenderAssets,
    render_phase::{
        BatchedPhaseItem, DrawFunctions, EntityRenderCommand, RenderCommand, RenderCommandResult,
        RenderPhase, SetItemPipeline, TrackedRenderPass,
    },
    render_resource::*,
    renderer::{RenderDevice, RenderQueue},
    texture::{BevyDefault, Image},
    view::{Msaa, ViewUniform, ViewUniformOffset, ViewUniforms, Visibility},
    Extract,
};
use bevy_transform::components::GlobalTransform;
use bevy_utils::FloatOrd;
use bevy_utils::HashMap;
use bytemuck::{Pod, Zeroable};
use copyless::VecHelper;

pub struct SpritePipeline {
    view_layout: BindGroupLayout,
    material_layout: BindGroupLayout,
}

impl FromWorld for SpritePipeline {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();

        let view_layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: Some(ViewUniform::min_size()),
                },
                count: None,
            }],
            label: Some("sprite_view_layout"),
        });

        let material_layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
            label: Some("sprite_material_layout"),
        });

        SpritePipeline {
            view_layout,
            material_layout,
        }
    }
}

bitflags::bitflags! {
    #[repr(transparent)]
    // NOTE: Apparently quadro drivers support up to 64x MSAA.
    // MSAA uses the highest 6 bits for the MSAA sample count - 1 to support up to 64x MSAA.
    pub struct SpritePipelineKey: u32 {
        const NONE                        = 0;
        const COLORED                     = (1 << 0);
        const MSAA_RESERVED_BITS          = SpritePipelineKey::MSAA_MASK_BITS << SpritePipelineKey::MSAA_SHIFT_BITS;
    }
}

impl SpritePipelineKey {
    const MSAA_MASK_BITS: u32 = 0b111111;
    const MSAA_SHIFT_BITS: u32 = 32 - 6;

    pub fn from_msaa_samples(msaa_samples: u32) -> Self {
        let msaa_bits = ((msaa_samples - 1) & Self::MSAA_MASK_BITS) << Self::MSAA_SHIFT_BITS;
        SpritePipelineKey::from_bits(msaa_bits).unwrap()
    }

    pub fn msaa_samples(&self) -> u32 {
        ((self.bits >> Self::MSAA_SHIFT_BITS) & Self::MSAA_MASK_BITS) + 1
    }
}

impl SpecializedRenderPipeline for SpritePipeline {
    type Key = SpritePipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        let mut formats = vec![
            // position
            VertexFormat::Float32x3,
            // uv
            VertexFormat::Float32x2,
        ];

        if key.contains(SpritePipelineKey::COLORED) {
            // color
            formats.push(VertexFormat::Float32x4);
        }

        let vertex_layout =
            VertexBufferLayout::from_vertex_formats(VertexStepMode::Vertex, formats);

        let mut shader_defs = Vec::new();
        if key.contains(SpritePipelineKey::COLORED) {
            shader_defs.push("COLORED".to_string());
        }

        RenderPipelineDescriptor {
            vertex: VertexState {
                shader: SPRITE_SHADER_HANDLE.typed::<Shader>(),
                entry_point: "vertex".into(),
                shader_defs: shader_defs.clone(),
                buffers: vec![vertex_layout],
            },
            fragment: Some(FragmentState {
                shader: SPRITE_SHADER_HANDLE.typed::<Shader>(),
                shader_defs,
                entry_point: "fragment".into(),
                targets: vec![ColorTargetState {
                    format: TextureFormat::bevy_default(),
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                }],
            }),
            layout: Some(vec![self.view_layout.clone(), self.material_layout.clone()]),
            primitive: PrimitiveState {
                front_face: FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
                topology: PrimitiveTopology::TriangleList,
                strip_index_format: None,
            },
            depth_stencil: None,
            multisample: MultisampleState {
                count: key.msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            label: Some("sprite_pipeline".into()),
        }
    }
}

pub struct ExtractedSprite {
    pub transform: GlobalTransform,
    pub color: Color,
    /// Select an area of the texture
    pub rect: Option<Rect>,
    /// Change the on-screen size of the sprite
    pub custom_size: Option<Vec2>,
    /// Handle to the `Image` of this sprite
    /// PERF: storing a `HandleId` instead of `Handle<Image>` enables some optimizations (`ExtractedSprite` becomes `Copy` and doesn't need to be dropped)
    pub image_handle_id: HandleId,
    pub flip_x: bool,
    pub flip_y: bool,
    pub anchor: Vec2,
}

#[derive(Default)]
pub struct ExtractedSprites {
    pub sprites: Vec<ExtractedSprite>,
}

#[derive(Default)]
pub struct SpriteAssetEvents {
    pub images: Vec<AssetEvent<Image>>,
}

pub fn extract_sprite_events(
    mut events: ResMut<SpriteAssetEvents>,
    mut image_events: Extract<EventReader<AssetEvent<Image>>>,
) {
    let SpriteAssetEvents { ref mut images } = *events;
    images.clear();

    for image in image_events.iter() {
        // AssetEvent: !Clone
        images.push(match image {
            AssetEvent::Created { handle } => AssetEvent::Created {
                handle: handle.clone_weak(),
            },
            AssetEvent::Modified { handle } => AssetEvent::Modified {
                handle: handle.clone_weak(),
            },
            AssetEvent::Removed { handle } => AssetEvent::Removed {
                handle: handle.clone_weak(),
            },
        });
    }
}

pub fn extract_sprites(
    mut extracted_sprites: ResMut<ExtractedSprites>,
    texture_atlases: Extract<Res<Assets<TextureAtlas>>>,
    sprite_query: Extract<Query<(&Visibility, &Sprite, &GlobalTransform, &Handle<Image>)>>,
    atlas_query: Extract<
        Query<(
            &Visibility,
            &TextureAtlasSprite,
            &GlobalTransform,
            &Handle<TextureAtlas>,
        )>,
    >,
) {
    for (visibility, sprite, transform, handle) in sprite_query.iter() {
        if !visibility.is_visible {
            continue;
        }
        // PERF: we don't check in this function that the `Image` asset is ready, since it should be in most cases and hashing the handle is expensive
        extracted_sprites.sprites.alloc().init(ExtractedSprite {
            color: sprite.color,
            transform: *transform,
            // Use the full texture
            rect: None,
            // Pass the custom size
            custom_size: sprite.custom_size,
            flip_x: sprite.flip_x,
            flip_y: sprite.flip_y,
            image_handle_id: handle.id,
            anchor: sprite.anchor.as_vec(),
        });
    }
    for (visibility, atlas_sprite, transform, texture_atlas_handle) in atlas_query.iter() {
        if !visibility.is_visible {
            continue;
        }
        if let Some(texture_atlas) = texture_atlases.get(texture_atlas_handle) {
            let rect = Some(texture_atlas.textures[atlas_sprite.index as usize]);
            extracted_sprites.sprites.alloc().init(ExtractedSprite {
                color: atlas_sprite.color,
                transform: *transform,
                // Select the area in the texture atlas
                rect,
                // Pass the custom size
                custom_size: atlas_sprite.custom_size,
                flip_x: atlas_sprite.flip_x,
                flip_y: atlas_sprite.flip_y,
                image_handle_id: texture_atlas.texture.id,
                anchor: atlas_sprite.anchor.as_vec(),
            });
        }
    }
}

/// Single sprite vertex data
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SpriteVertex {
    /// 3D vertex position
    pub position: [f32; 3],
    /// vertex UV coordinates
    pub uv: [f32; 2],
}

/// Single sprite colored vertex data
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ColoredSpriteVertex {
    /// 3D vertex position
    pub position: [f32; 3],
    /// Vertex UV coordinates
    pub uv: [f32; 2],
    /// Vertex color as linear RGBA
    pub color: [f32; 4],
}

/// Sprite rendering information, stored as a resource
pub struct SpriteMeta {
    /// Non colored vertices buffer
    vertices: BufferVec<SpriteVertex>,
    /// Colored vertices buffer
    colored_vertices: BufferVec<ColoredSpriteVertex>,
    /// The associated pipeline's bind group
    view_bind_group: Option<BindGroup>,
}

impl Default for SpriteMeta {
    fn default() -> Self {
        Self {
            vertices: BufferVec::new(BufferUsages::VERTEX),
            colored_vertices: BufferVec::new(BufferUsages::VERTEX),
            view_bind_group: None,
        }
    }
}

/// Sprite quad triangles vertex indices
const QUAD_INDICES: [usize; 6] = [
    0, 2, 3, // Bottom left triangle
    0, 1, 2, // Top right triangle
];

/// Base Quad vertices 2D positions
const QUAD_VERTEX_POSITIONS: [Vec2; 4] = [
    // Top left
    Vec2::new(-0.5, -0.5),
    // Top right
    Vec2::new(0.5, -0.5),
    // Bottom right
    Vec2::new(0.5, 0.5),
    // Bottom left
    Vec2::new(-0.5, 0.5),
];

/// Base Quad vertices UV coordinates
const QUAD_UVS: [Vec2; 4] = [
    // Top left
    Vec2::new(0., 1.),
    // Top right
    Vec2::new(1., 1.),
    // Bottom right
    Vec2::new(1., 0.),
    // Bottom left
    Vec2::new(0., 0.),
];

/// Component defining a batch of sprites for the render world
#[derive(Component)]
pub struct SpriteBatch {
    /// The [`SpriteMeta`] vertex data indices
    range: Range<u32>,
    /// The texture handle id
    image_handle_id: HandleId,
    /// Defines if the `range` targets [`SpriteMeta::vertices`] or [`SpriteMeta::colored_vertices`]
    colored: bool,
    /// Sort key of the batch
    z_order: f32,
}

pub fn prepare_sprites(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut sprite_meta: ResMut<SpriteMeta>,
    gpu_images: Res<RenderAssets<Image>>,
    mut extracted_sprites: ResMut<ExtractedSprites>,
) {
    // Clear the vertex buffers
    sprite_meta.vertices.clear();
    sprite_meta.colored_vertices.clear();

    // Sort sprites by z for correct transparency and then by handle to improve batching
    extracted_sprites.sprites.sort_unstable_by(|a, b| {
        match a
            .transform
            .translation
            .z
            .partial_cmp(&b.transform.translation.z)
        {
            Some(Ordering::Equal) | None => a.image_handle_id.cmp(&b.image_handle_id),
            Some(other) => other,
        }
    });

    // Impossible starting values that will be replaced on the first iteration
    let mut current_batch_handle = HandleId::Id(Uuid::nil(), u64::MAX);
    let mut current_image_size = Vec2::ZERO;
    let mut current_batch_colored = false;
    let mut z_order = 0.0;

    // Vertex buffer indices
    let [mut white_start, mut white_end] = [0, 0];
    let [mut colored_start, mut colored_end] = [0, 0];

    for extracted_sprite in extracted_sprites.sprites.drain(..) {
        let colored = extracted_sprite.color != Color::WHITE;
        if extracted_sprite.image_handle_id != current_batch_handle
            || colored != current_batch_colored
        {
            if let Some(gpu_image) = gpu_images.get(&Handle::weak(extracted_sprite.image_handle_id))
            {
                current_image_size = gpu_image.size;
                current_batch_handle = extracted_sprite.image_handle_id;
                current_batch_colored = colored;
                let [start, end] = match colored {
                    true => [&mut colored_start, &mut colored_end],
                    false => [&mut white_start, &mut white_end],
                };
                if *start != *end {
                    commands.spawn().insert(SpriteBatch {
                        range: *start..*end,
                        image_handle_id: current_batch_handle,
                        colored: current_batch_colored,
                        z_order,
                    });
                    *start = *end;
                }
            } else {
                // We skip loading images
                continue;
            }
        }
        // Calculate vertex data for this item
        let mut uvs = QUAD_UVS;
        if extracted_sprite.flip_x {
            uvs = [uvs[1], uvs[0], uvs[3], uvs[2]];
        }
        if extracted_sprite.flip_y {
            uvs = [uvs[3], uvs[2], uvs[1], uvs[0]];
        }

        // By default, the size of the quad is the size of the texture
        let mut quad_size = current_image_size;

        // If a rect is specified, adjust UVs and the size of the quad
        if let Some(rect) = extracted_sprite.rect {
            let rect_size = rect.size();
            for uv in &mut uvs {
                *uv = (rect.min + *uv * rect_size) / current_image_size;
            }
            quad_size = rect_size;
        }

        // Override the size if a custom one is specified
        if let Some(custom_size) = extracted_sprite.custom_size {
            quad_size = custom_size;
        }

        // Apply size and global transform
        let positions = QUAD_VERTEX_POSITIONS.map(|quad_pos| {
            extracted_sprite
                .transform
                .mul_vec3(((quad_pos - extracted_sprite.anchor) * quad_size).extend(0.))
                .into()
        });
        if colored {
            for i in QUAD_INDICES {
                sprite_meta.colored_vertices.push(ColoredSpriteVertex {
                    position: positions[i],
                    uv: uvs[i].into(),
                    color: extracted_sprite.color.as_linear_rgba_f32(),
                });
            }
            colored_end += QUAD_INDICES.len() as u32;
        } else {
            for i in QUAD_INDICES {
                sprite_meta.vertices.push(SpriteVertex {
                    position: positions[i],
                    uv: uvs[i].into(),
                });
            }
            white_end += QUAD_INDICES.len() as u32;
        }
        z_order = extracted_sprite.transform.translation.z;
    }
    // if start != end, there is one last batch to process
    let [start, end] = match current_batch_colored {
        true => [&mut colored_start, &mut colored_end],
        false => [&mut white_start, &mut white_end],
    };
    if *start != *end {
        commands.spawn().insert(SpriteBatch {
            range: *start..*end,
            image_handle_id: current_batch_handle,
            colored: current_batch_colored,
            z_order,
        });
    }

    sprite_meta
        .vertices
        .write_buffer(&render_device, &render_queue);
    sprite_meta
        .colored_vertices
        .write_buffer(&render_device, &render_queue);
}

#[derive(Default)]
pub struct ImageBindGroups {
    values: HashMap<Handle<Image>, BindGroup>,
}

#[allow(clippy::too_many_arguments)]
pub fn queue_sprites(
    draw_functions: Res<DrawFunctions<Transparent2d>>,
    render_device: Res<RenderDevice>,
    mut sprite_meta: ResMut<SpriteMeta>,
    view_uniforms: Res<ViewUniforms>,
    sprite_pipeline: Res<SpritePipeline>,
    mut pipelines: ResMut<SpecializedRenderPipelines<SpritePipeline>>,
    mut pipeline_cache: ResMut<PipelineCache>,
    mut image_bind_groups: ResMut<ImageBindGroups>,
    gpu_images: Res<RenderAssets<Image>>,
    msaa: Res<Msaa>,
    sprite_batches: Query<(Entity, &SpriteBatch)>,
    mut views: Query<&mut RenderPhase<Transparent2d>>,
    events: Res<SpriteAssetEvents>,
) {
    // If an image has changed, the GpuImage has (probably) changed
    for event in &events.images {
        match event {
            AssetEvent::Created { .. } => None,
            AssetEvent::Modified { handle } | AssetEvent::Removed { handle } => {
                image_bind_groups.values.remove(handle)
            }
        };
    }

    if let Some(view_binding) = view_uniforms.uniforms.binding() {
        sprite_meta.view_bind_group = Some(render_device.create_bind_group(&BindGroupDescriptor {
            entries: &[BindGroupEntry {
                binding: 0,
                resource: view_binding,
            }],
            label: Some("sprite_view_bind_group"),
            layout: &sprite_pipeline.view_layout,
        }));

        let draw_sprite_function = draw_functions.read().get_id::<DrawSprite>().unwrap();
        let key = SpritePipelineKey::from_msaa_samples(msaa.samples);
        let pipeline = pipelines.specialize(&mut pipeline_cache, &sprite_pipeline, key);
        let colored_pipeline = pipelines.specialize(
            &mut pipeline_cache,
            &sprite_pipeline,
            key | SpritePipelineKey::COLORED,
        );

        // FIXME: VisibleEntities is ignored
        for mut transparent_phase in &mut views {
            let image_bind_groups = &mut *image_bind_groups;
            for (entity, sprite_batch) in sprite_batches.iter() {
                image_bind_groups
                    .values
                    .entry(Handle::weak(sprite_batch.image_handle_id))
                    .or_insert_with(|| {
                        let gpu_image = gpu_images
                            .get(&Handle::weak(sprite_batch.image_handle_id))
                            .unwrap();
                        render_device.create_bind_group(&BindGroupDescriptor {
                            entries: &[
                                BindGroupEntry {
                                    binding: 0,
                                    resource: BindingResource::TextureView(&gpu_image.texture_view),
                                },
                                BindGroupEntry {
                                    binding: 1,
                                    resource: BindingResource::Sampler(&gpu_image.sampler),
                                },
                            ],
                            label: Some("sprite_material_bind_group"),
                            layout: &sprite_pipeline.material_layout,
                        })
                    });

                // These items will be sorted by depth with other phase items
                let sort_key = FloatOrd(sprite_batch.z_order);

                // Store the vertex data and add the item to the render phase
                if sprite_batch.colored {
                    transparent_phase.add(Transparent2d {
                        draw_function: draw_sprite_function,
                        pipeline: colored_pipeline,
                        entity,
                        sort_key,
                        batch_range: Some(sprite_batch.range.clone()),
                    });
                } else {
                    transparent_phase.add(Transparent2d {
                        draw_function: draw_sprite_function,
                        pipeline,
                        entity,
                        sort_key,
                        batch_range: Some(sprite_batch.range.clone()),
                    });
                }
            }
        }
    }
}

pub type DrawSprite = (
    SetItemPipeline,
    SetSpriteViewBindGroup<0>,
    SetSpriteTextureBindGroup<1>,
    DrawSpriteBatch,
);

pub struct SetSpriteViewBindGroup<const I: usize>;
impl<const I: usize> EntityRenderCommand for SetSpriteViewBindGroup<I> {
    type Param = (SRes<SpriteMeta>, SQuery<Read<ViewUniformOffset>>);

    fn render<'w>(
        view: Entity,
        _item: Entity,
        (sprite_meta, view_query): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let view_uniform = view_query.get(view).unwrap();
        pass.set_bind_group(
            I,
            sprite_meta.into_inner().view_bind_group.as_ref().unwrap(),
            &[view_uniform.offset],
        );
        RenderCommandResult::Success
    }
}
pub struct SetSpriteTextureBindGroup<const I: usize>;
impl<const I: usize> EntityRenderCommand for SetSpriteTextureBindGroup<I> {
    type Param = (SRes<ImageBindGroups>, SQuery<Read<SpriteBatch>>);

    fn render<'w>(
        _view: Entity,
        item: Entity,
        (image_bind_groups, query_batch): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let sprite_batch = query_batch.get(item).unwrap();
        let image_bind_groups = image_bind_groups.into_inner();

        pass.set_bind_group(
            I,
            image_bind_groups
                .values
                .get(&Handle::weak(sprite_batch.image_handle_id))
                .unwrap(),
            &[],
        );
        RenderCommandResult::Success
    }
}

pub struct DrawSpriteBatch;
impl<P: BatchedPhaseItem> RenderCommand<P> for DrawSpriteBatch {
    type Param = (SRes<SpriteMeta>, SQuery<Read<SpriteBatch>>);

    fn render<'w>(
        _view: Entity,
        item: &P,
        (sprite_meta, query_batch): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let sprite_batch = query_batch.get(item.entity()).unwrap();
        let sprite_meta = sprite_meta.into_inner();
        if sprite_batch.colored {
            pass.set_vertex_buffer(0, sprite_meta.colored_vertices.buffer().unwrap().slice(..));
        } else {
            pass.set_vertex_buffer(0, sprite_meta.vertices.buffer().unwrap().slice(..));
        }
        pass.draw(item.batch_range().as_ref().unwrap().clone(), 0..1);
        RenderCommandResult::Success
    }
}

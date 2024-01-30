#import bevy_render::view::View

const TEXTURED_QUAD: u32 = 0u;

@group(0) @binding(0) var<uniform> view: View;

struct VertexOutput {
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(3) @interpolate(flat) mode: u32,
    @builtin(position) position: vec4<f32>,
};

@vertex
fn vertex(
    @location(0) vertex_position: vec3<f32>,
    @location(1) vertex_uv: vec2<f32>,
    @location(2) vertex_color: vec4<f32>,
    @location(3) mode: u32,
) -> VertexOutput {
    var out: VertexOutput;
    out.uv = vertex_uv;
    out.position = view.view_proj * vec4<f32>(vertex_position, 1.0);
    out.color = vertex_color;
    out.mode = mode;
    return out;
}

@group(1) @binding(0) var sprite_texture: texture_2d<f32>;
@group(1) @binding(1) var sprite_sampler: sampler;

struct TextureScaler {
    @location(0) border: vec4<f32>,
    @location(1) tiling_factor: vec2<f32>
}

@group(2) @binding(0) var<uniform> scaler: TextureScaler;

fn map(value: f32, min: f32, max: f32, new_min: f32, new_max: f32) -> f32 {
    return (value - min) / (max - min) * (new_max - new_min) + new_min;
}

fn slice_axis(coord: f32, tx_border_min: f32, tx_border_max: f32, win_border_min: f32, win_border_max: f32) -> f32 {
    if coord < win_border_min {
        return map(coord, 0.0, win_border_min, 0.0, tx_border_min);
    }
    if coord < win_border_max {
        return map(coord, win_border_min, win_border_max, tx_border_min, tx_border_max);
    }
    return map(coord, win_border_max, 1.0, tx_border_max, 1.0);
}

fn tile_texture(coord: f32, tx_border_min: f32, tx_border_max: f32, tiling_factor: f32) -> f32 {
    if coord >= tx_border_min && coord <= tx_border_max {
        return fract(coord * tiling_factor);
    }
    return coord;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // We retrieve the texture dimensions
    let u_dims: vec2<u32> = textureDimensions(sprite_texture);
    let dimensions: vec2<f32> = vec2<f32>(f32(u_dims.x), f32(u_dims.y));

    var uv = in.uv;
    // Apply slicing
    uv.x = slice_axis(uv.x, scaler.border.x, 1.0 - scaler.border.z, dimensions.x, 1.0 - dimensions.x);
    uv.y = slice_axis(uv.y, scaler.border.y, 1.0 - scaler.border.w, dimensions.y, 1.0 - dimensions.y);
    // Apply tiling
    uv.x = tile_texture(uv.x, scaler.border.x, 1.0 - scaler.border.z, scaler.tiling_factor.x);
    uv.y = tile_texture(uv.y, scaler.border.y, 1.0 - scaler.border.w, scaler.tiling_factor.y);

    // textureSample can only be called in unform control flow, not inside an if branch.
    var color = textureSample(sprite_texture, sprite_sampler, uv);
    if in.mode == TEXTURED_QUAD {
        color = in.color * color;
    } else {
        color = in.color;
    }
    return color;
}

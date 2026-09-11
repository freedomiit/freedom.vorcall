//! The screen-share stage: the toolbar over the video, and the wgpu path that
//! turns a decoded I420 picture into pixels.
//!
//! Nothing here reads the application state. The caller hands over the picture
//! and a set of message constructors, so the same stage is drawn in the main
//! window and in the popped-out one.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use iced::alignment::Vertical;
use iced::widget::{button, column, container, pick_list, row, shader, slider, text};
use iced::{Element, Length, Rectangle, Theme, mouse, wgpu};
use vorcall_screen::codec::Picture;

use crate::brand::palette::{GROUND, MUTED};

/// BT.709 limited range, the range the encoder writes. Black sits at 16 and
/// white at 235 on the luma plane, neutral chroma at 128; the coefficients are
/// the ones the standard tabulates. The WGSL below is formatted out of these
/// same constants so the shader and the tests cannot drift apart.
const Y_OFFSET: f32 = 16.0 / 255.0;
const Y_SCALE: f32 = 255.0 / 219.0;
const C_OFFSET: f32 = 128.0 / 255.0;
const C_SCALE: f32 = 255.0 / 224.0;
const R_CR: f32 = 1.5748;
const G_CB: f32 = -0.1873;
const G_CR: f32 = -0.4681;
const B_CB: f32 = 1.8556;

/// The width the volume slider gets in the toolbar.
const VOLUME_WIDTH: f32 = 120.0;

/// What the stage sends back. The caller owns the message type, so the stage
/// can be built before those variants exist.
pub struct StageHandlers<M: Clone> {
    pub watch: fn(i64) -> M,
    pub stop: M,
    pub pop_out: M,
    pub pop_in: M,
    pub fullscreen: M,
    pub volume: fn(f32) -> M,
    pub volume_released: M,
}

/// Everything the stage draws, read out of the application state by the caller.
pub struct StageView<'a> {
    pub sharer: &'a str,
    pub sharers: Vec<(i64, String)>,
    pub current: i64,
    pub picture: Option<&'a Arc<Picture>>,
    pub seq: u64,
    pub has_audio: bool,
    pub volume: f32,
    pub stats: String,
    pub popped: bool,
    pub fullscreen: bool,
}

/// The in-window stage: toolbar above the video. In fullscreen only the video
/// and a slim toolbar remain.
pub fn view<'a, M: Clone + 'a>(stage: StageView<'a>, handlers: StageHandlers<M>) -> Element<'a, M> {
    let popped = stage.popped;
    content(stage, handlers, popped)
}

/// The pop-out window's content: the same toolbar (Pop in instead of Pop out)
/// and video.
pub fn popped<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
) -> Element<'a, M> {
    content(stage, handlers, true)
}

fn content<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
    popped: bool,
) -> Element<'a, M> {
    let video = video(stage.picture, stage.seq);

    column![toolbar(stage, handlers, popped), video]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn toolbar<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
    popped: bool,
) -> Element<'a, M> {
    let StageHandlers {
        watch,
        stop,
        pop_out,
        pop_in,
        fullscreen,
        volume,
        volume_released,
    } = handlers;

    let mut bar = row![text(format!("Watching {}", stage.sharer))]
        .spacing(8)
        .align_y(Vertical::Center);

    // In fullscreen the video is the whole window: only the controls that get
    // the viewer back out of it, and the volume, stay. With a single sharer
    // there is nothing to pick between, so the name above is the whole story.
    if !stage.fullscreen && stage.sharers.len() > 1 {
        let options: Vec<Sharer> = stage
            .sharers
            .into_iter()
            .map(|(id, username)| Sharer { id, username })
            .collect();
        let selected = options
            .iter()
            .find(|sharer| sharer.id == stage.current)
            .cloned();

        bar = bar.push(pick_list(options, selected, move |sharer| watch(sharer.id)));
    }

    if stage.has_audio {
        bar = bar.push(
            slider(0.0..=2.0, stage.volume, volume)
                .step(0.05_f32)
                .on_release(volume_released)
                .width(VOLUME_WIDTH),
        );
        bar = bar.push(text(format!("{:.0}%", stage.volume * 100.0)).color(MUTED));
    }

    if !stage.fullscreen {
        bar = bar.push(text(stage.stats).color(MUTED));
        bar = bar.push(
            button(text(if popped { "Pop in" } else { "Pop out" })).on_press(if popped {
                pop_in
            } else {
                pop_out
            }),
        );
    }

    bar = bar.push(
        button(text(if stage.fullscreen {
            "Exit fullscreen"
        } else {
            "Fullscreen"
        }))
        .on_press(fullscreen),
    );
    bar = bar.push(
        button(text("Stop watching"))
            .on_press(stop)
            .style(button::danger),
    );

    container(bar).padding(8).width(Length::Fill).into()
}

fn video<'a, M: 'a>(picture: Option<&'a Arc<Picture>>, seq: u64) -> Element<'a, M> {
    let body: Element<'a, M> = match picture {
        Some(picture) => shader(StageProgram {
            picture: Some(picture.clone()),
            seq,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        None => text("Waiting for video…").color(MUTED).into(),
    };

    container(body).center(Length::Fill).style(ground).into()
}

/// The letterbox bars are this container's background: the shader only paints
/// inside the aspect-fitted rectangle.
fn ground(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(GROUND.into()),
        ..container::Style::default()
    }
}

/// A pick-list entry. The list shows the username and hands back the id.
#[derive(Clone, PartialEq, Eq)]
struct Sharer {
    id: i64,
    username: String,
}

impl fmt::Display for Sharer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.username)
    }
}

pub struct StageProgram {
    pub picture: Option<Arc<Picture>>,
    pub seq: u64,
}

impl<M> shader::Program<M> for StageProgram {
    type State = ();
    type Primitive = StagePrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> StagePrimitive {
        StagePrimitive {
            picture: self.picture.clone(),
            seq: self.seq,
        }
    }
}

pub struct StagePrimitive {
    picture: Option<Arc<Picture>>,
    seq: u64,
}

// Hand-written so a frame never reaches a log as pixels.
impl fmt::Debug for StagePrimitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagePrimitive")
            .field("seq", &self.seq)
            .field("width", &self.picture.as_ref().map_or(0, |p| p.width))
            .field("height", &self.picture.as_ref().map_or(0, |p| p.height))
            .finish()
    }
}

impl shader::Primitive for StagePrimitive {
    type Pipeline = StagePipeline;

    fn prepare(
        &self,
        pipeline: &mut StagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        let Some(picture) = self.picture.as_deref() else {
            return;
        };
        if picture.width == 0 || picture.height == 0 {
            return;
        }

        let stale = pipeline
            .planes
            .as_ref()
            .is_none_or(|planes| planes.width != picture.width || planes.height != picture.height);
        if stale {
            pipeline.planes = Some(Planes::new(
                device,
                &pipeline.layout,
                &pipeline.sampler,
                picture.width,
                picture.height,
            ));
            pipeline.uploaded_seq = None;
        }

        let Some(planes) = pipeline.planes.as_ref() else {
            return;
        };

        // The primitive is rebuilt on every view(); only a new decoded picture
        // is worth the three uploads.
        if pipeline.uploaded_seq != Some(self.seq) {
            let chroma_width = picture.width.div_ceil(2);
            let chroma_height = picture.height.div_ceil(2);

            upload(
                queue,
                &planes.y,
                &picture.y,
                picture.y_stride,
                picture.width,
                picture.height,
            );
            upload(
                queue,
                &planes.u,
                &picture.u,
                picture.uv_stride,
                chroma_width,
                chroma_height,
            );
            upload(
                queue,
                &planes.v,
                &picture.v,
                picture.uv_stride,
                chroma_width,
                chroma_height,
            );

            pipeline.uploaded_seq = Some(self.seq);
        }

        // iced draws with the render pass's viewport already set to the widget
        // in physical pixels; the fit is the same rectangle, shrunk to the
        // picture's aspect ratio.
        pipeline.fit = Some(fit_rect(
            *bounds * viewport.scale_factor(),
            picture.width,
            picture.height,
        ));
    }

    fn draw(&self, pipeline: &StagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let (Some(planes), Some(fit)) = (pipeline.planes.as_ref(), pipeline.fit) else {
            return false;
        };
        if fit.width < 1.0 || fit.height < 1.0 {
            return false;
        }

        render_pass.set_pipeline(&pipeline.render);
        render_pass.set_bind_group(0, &planes.bind_group, &[]);
        render_pass.set_viewport(fit.x, fit.y, fit.width, fit.height, 0.0, 1.0);
        render_pass.draw(0..3, 0..1);

        true
    }
}

/// One per renderer engine, and one stage is on screen per window, so the
/// planes below belong to whatever picture was last prepared.
pub struct StagePipeline {
    render: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    planes: Option<Planes>,
    uploaded_seq: Option<u64>,
    fit: Option<Rectangle>,
}

impl shader::Pipeline for StagePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vorcall stage sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            min_filter: wgpu::FilterMode::Linear,
            mag_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let plane = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vorcall stage planes layout"),
            entries: &[
                plane(0),
                plane(1),
                plane(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vorcall stage pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vorcall stage shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(wgsl())),
        });

        let render = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vorcall stage pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        Self {
            render,
            sampler,
            layout,
            planes: None,
            uploaded_seq: None,
            fit: None,
        }
    }

    // trim() is left at its no-op default: dropping the planes between frames
    // would cost a full re-upload on the next one, and they go with the
    // pipeline when the window does.
}

struct Planes {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl Planes {
    fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
    ) -> Self {
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);

        let y = plane_texture(device, "vorcall stage Y plane", width, height);
        let u = plane_texture(device, "vorcall stage U plane", chroma_width, chroma_height);
        let v = plane_texture(device, "vorcall stage V plane", chroma_width, chroma_height);

        let views =
            [&y, &u, &v].map(|plane| plane.create_view(&wgpu::TextureViewDescriptor::default()));

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vorcall stage planes"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });

        Self {
            y,
            u,
            v,
            bind_group,
            width,
            height,
        }
    }
}

fn plane_texture(device: &wgpu::Device, label: &str, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn upload(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    plane: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) {
    let rows = height as usize;
    // A padded stride is what the decoder normally hands out; a stride shorter
    // than the row cannot happen for I420, and a short plane would be rejected
    // by wgpu's own validation, so both are dropped rather than drawn.
    if stride < width as usize || rows == 0 {
        return;
    }
    let needed = stride * (rows - 1) + width as usize;
    if plane.len() < needed {
        return;
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &plane[..needed],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride as u32),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    )
}

/// The largest `width:height` rectangle inside `area`, centred, in whole
/// physical pixels.
fn fit_rect(area: Rectangle, width: u32, height: u32) -> Rectangle {
    if area.width <= 0.0 || area.height <= 0.0 || width == 0 || height == 0 {
        return Rectangle {
            x: area.x,
            y: area.y,
            width: 0.0,
            height: 0.0,
        };
    }

    let scale = (area.width / width as f32).min(area.height / height as f32);
    let fitted_width = (width as f32 * scale).floor().max(1.0);
    let fitted_height = (height as f32 * scale).floor().max(1.0);

    Rectangle {
        x: (area.x + (area.width - fitted_width) / 2.0).round(),
        y: (area.y + (area.height - fitted_height) / 2.0).round(),
        width: fitted_width,
        height: fitted_height,
    }
}

/// The full-screen triangle and the BT.709 conversion, with the constants above
/// formatted in so the shader cannot drift from the table the tests read.
fn wgsl() -> String {
    format!(
        "\
@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var u_plane: texture_2d<f32>;
@group(0) @binding(2) var v_plane: texture_2d<f32>;
@group(0) @binding(3) var plane_sampler: sampler;

struct Output {{
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Output {{
    var out: Output;
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    out.clip = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}}

@fragment
fn fs_main(frag: Output) -> @location(0) vec4<f32> {{
    let luma = (textureSample(y_plane, plane_sampler, frag.uv).r - ({y_offset:?})) * ({y_scale:?});
    let cb = (textureSample(u_plane, plane_sampler, frag.uv).r - ({c_offset:?})) * ({c_scale:?});
    let cr = (textureSample(v_plane, plane_sampler, frag.uv).r - ({c_offset:?})) * ({c_scale:?});
    let rgb = vec3<f32>(
        luma + ({r_cr:?}) * cr,
        luma + ({g_cb:?}) * cb + ({g_cr:?}) * cr,
        luma + ({b_cb:?}) * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}}
",
        y_offset = Y_OFFSET,
        y_scale = Y_SCALE,
        c_offset = C_OFFSET,
        c_scale = C_SCALE,
        r_cr = R_CR,
        g_cb = G_CB,
        g_cr = G_CR,
        b_cb = B_CB,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fragment stage, in Rust: the guard that the constants the WGSL is
    /// built from are the ones BT.709 limited range asks for.
    fn yuv_to_rgb(y: f32, u: f32, v: f32) -> [f32; 3] {
        let luma = (y - Y_OFFSET) * Y_SCALE;
        let cb = (u - C_OFFSET) * C_SCALE;
        let cr = (v - C_OFFSET) * C_SCALE;

        [
            luma + R_CR * cr,
            luma + G_CB * cb + G_CR * cr,
            luma + B_CB * cb,
        ]
    }

    #[test]
    fn a_16_9_picture_in_a_4_3_box_is_letterboxed_and_centred() {
        let fit = fit_rect(
            Rectangle {
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
            },
            1920,
            1080,
        );

        assert_eq!(
            (fit.x, fit.y, fit.width, fit.height),
            (10.0, 95.0, 800.0, 450.0)
        );
    }

    #[test]
    fn a_tall_picture_is_pillarboxed() {
        let fit = fit_rect(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 600.0,
            },
            600,
            1200,
        );

        assert_eq!(
            (fit.x, fit.y, fit.width, fit.height),
            (300.0, 0.0, 300.0, 600.0)
        );
    }

    #[test]
    fn a_zero_box_yields_a_zero_rect() {
        let empty = fit_rect(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            1920,
            1080,
        );
        assert_eq!((empty.width, empty.height), (0.0, 0.0));

        let no_picture = fit_rect(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            0,
            0,
        );
        assert_eq!((no_picture.width, no_picture.height), (0.0, 0.0));
    }

    #[test]
    fn black_16_maps_to_zero_and_white_235_to_one() {
        // BT.709 limited range: luma 16 is black, 235 is white, 128 is neutral
        // chroma. Those three numbers come from the standard, not from the
        // formula under test.
        let black = yuv_to_rgb(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0);
        let white = yuv_to_rgb(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0);

        for channel in black {
            assert!(channel.abs() < 1e-4, "black channel {channel}");
        }
        for channel in white {
            assert!((channel - 1.0).abs() < 1e-4, "white channel {channel}");
        }
    }

    #[test]
    fn the_wgsl_declares_both_entry_points_and_the_three_planes() {
        let source = wgsl();

        assert!(source.contains("fn vs_main"), "{source}");
        assert!(source.contains("fn fs_main"), "{source}");

        for plane in ["y_plane", "u_plane", "v_plane"] {
            assert!(
                source.contains(&format!("var {plane}: texture_2d<f32>")),
                "{source}"
            );
        }
        assert_eq!(source.matches("texture_2d<f32>").count(), 3, "{source}");

        // The conversion the CPU reference above checks is the one the shader
        // carries.
        assert!(source.contains(&format!("({R_CR:?})")), "{source}");
        assert!(source.contains(&format!("({G_CB:?})")), "{source}");
    }
}

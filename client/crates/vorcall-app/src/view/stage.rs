//! The stage: a watched screen share, the cameras beside it, the toolbar over
//! the lot, and the wgpu path that turns a decoded I420 picture into pixels.
//!
//! [`view`] and [`popped`] read nothing but [`StageView`] and [`StageHandlers`],
//! so the same stage is drawn in the chat column and in the popped-out window;
//! [`in_chat`] and [`popped_window`] are what fill those in from the application
//! state.
//!
//! Several pictures are on screen at once, and one renderer draws all of them
//! through a single [`StagePipeline`] — so the planes are cached per tile rather
//! than per renderer, and every primitive says which tile it is.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Space, button, column, container, mouse_area, pick_list, row, shader, slider, stack, svg, text,
    tooltip,
};
use iced::{Color, Element, Length, Padding, Rectangle, mouse, wgpu};
use vorcall_screen::codec::Picture;

use crate::app::message::{CameraMsg, Message, ShareMsg};
use crate::app::state::rules::{self, SHARE_VOLUME_MAX};
use crate::app::state::voice::CameraTileId;
use crate::app::{App, MainState};
use crate::icons::Icon;
use crate::theme::{ThemeTokens, styles};
use crate::view::widgets::ICON_SIZE;
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_SECONDARY};

/// BT.709 limited range, the range the encoder writes. Black sits at 16 and white
/// at 235 on the luma plane, neutral chroma at 128; the coefficients are the ones
/// the standard tabulates. The WGSL below is formatted out of these same
/// constants so the shader and the tests cannot drift apart.
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

/// The padding around an icon in the toolbar, matching the icon buttons
/// everywhere else.
const ICON_PADDING: f32 = 6.0;

/// How tall the camera strip under a large picture is. The tiles are 16:9, so
/// this is what decides their width as well.
const STRIP_HEIGHT: f32 = 132.0;

/// The gap between two tiles, and the inset of a tile's own name.
const TILE_SPACING: f32 = 6.0;

/// Where the grid goes to two columns: one tile fills the stage, two sit side by
/// side, and everything past that is a grid.
const GRID_COLUMNS: usize = 2;

/// How many tiles' textures one renderer keeps. At most a share, this client's
/// own preview and four watched cameras are ever on screen; the rest are peers
/// who have come and gone, and their planes are worth evicting rather than
/// holding for the window's life.
const MAX_CACHED_TILES: usize = 8;

/// What the stage sends back. The caller owns the message type, so the stage can
/// be drawn from any of them.
pub struct StageHandlers<M: Clone> {
    pub watch: fn(i64) -> M,
    /// Stop watching the share, or — with no share on the stage — every camera.
    pub stop: M,
    pub stop_cameras: M,
    pub pop_out: M,
    pub pop_in: M,
    pub fullscreen: M,
    pub feature: fn(CameraTileId) -> M,
    pub volume: fn(f32) -> M,
    pub volume_released: M,
}

/// One camera on the stage, as the caller read it out of the state.
pub struct StageTile<'a> {
    pub id: CameraTileId,
    /// The member's own name; this client's preview says "You".
    pub name: Cow<'a, str>,
    pub picture: Option<&'a Arc<Picture>>,
    pub seq: u64,
}

/// Everything the stage draws, read out of the application state by the caller.
pub struct StageView<'a> {
    pub sharer: &'a str,
    pub sharers: Vec<(i64, String)>,
    pub current: i64,
    /// The watched share's newest picture, when one is being watched at all.
    pub picture: Option<&'a Arc<Picture>>,
    pub seq: u64,
    /// Whether a share is on the stage, which a share still waiting for its
    /// first picture also is.
    pub sharing: bool,
    /// This client's own preview first, then the watched cameras by name.
    pub cameras: Vec<StageTile<'a>>,
    /// The camera a press promoted into the large picture.
    pub featured: Option<CameraTileId>,
    pub has_audio: bool,
    pub volume: f32,
    pub stats: String,
    pub popped: bool,
    pub fullscreen: bool,
}

/// How the stage lays its pictures out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageLayout {
    /// Nothing to draw at all.
    Empty,
    /// One large picture with `strip` tiles in a row beneath it.
    Featured { strip: usize },
    /// Tiles only, filling the stage.
    Grid { rows: usize, columns: usize },
}

/// Where `tiles` pictures go: `featured` is a share on the stage, or a camera a
/// press promoted, and it always takes the large picture with the rest in a row
/// beneath. With no featured picture the tiles fill the stage — one alone, two
/// side by side, and two columns past that.
pub fn stage_layout(featured: bool, tiles: usize) -> StageLayout {
    if featured {
        return StageLayout::Featured { strip: tiles };
    }
    match tiles {
        0 => StageLayout::Empty,
        1 => StageLayout::Grid {
            rows: 1,
            columns: 1,
        },
        2 => StageLayout::Grid {
            rows: 1,
            columns: GRID_COLUMNS,
        },
        more => StageLayout::Grid {
            rows: more.div_ceil(GRID_COLUMNS),
            columns: GRID_COLUMNS,
        },
    }
}

/// The in-window stage: toolbar above the video.
pub fn view<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    let popped = stage.popped;
    content(stage, handlers, tokens, popped)
}

/// The pop-out window's content: the same toolbar, spelled Pop in.
pub fn popped<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    content(stage, handlers, tokens, true)
}

fn content<'a, M: Clone + 'a>(
    mut stage: StageView<'a>,
    handlers: StageHandlers<M>,
    tokens: &'a ThemeTokens,
    popped: bool,
) -> Element<'a, M> {
    let cameras = std::mem::take(&mut stage.cameras);
    let on_stage = cameras.len();
    let body = body(&stage, cameras, handlers.feature, tokens);
    column![toolbar(stage, handlers, on_stage, tokens, popped), body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The pictures themselves, filling whatever is left under the toolbar.
fn body<'a, M: Clone + 'a>(
    stage: &StageView<'a>,
    cameras: Vec<StageTile<'a>>,
    feature: fn(CameraTileId) -> M,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    // A press on the promoted tile puts it back in the row, and one on any other
    // swaps it into the large picture.
    let mut strip: Vec<Element<'a, M>> = Vec::new();
    let mut featured: Option<Element<'a, M>> = None;

    for tile in cameras {
        let promoted = stage.featured == Some(tile.id);
        let press = feature(tile.id);
        let drawn = camera_tile(tile, press, tokens);
        if promoted && featured.is_none() {
            featured = Some(drawn);
        } else {
            strip.push(drawn);
        }
    }

    if stage.sharing {
        // A promoted camera takes the large picture and the share joins the row,
        // which is the only way round the two ever swap.
        let share = share_tile(stage, tokens);
        match featured {
            Some(_) => strip.insert(0, share),
            None => featured = Some(share),
        }
    }

    match stage_layout(featured.is_some(), strip.len()) {
        StageLayout::Empty => ground(waiting(tokens), tokens),
        StageLayout::Featured { strip: 0 } => {
            ground(featured.unwrap_or_else(|| waiting(tokens)), tokens)
        }
        StageLayout::Featured { .. } => {
            let large = featured.unwrap_or_else(|| waiting(tokens));
            // The strip keeps its height whatever the window does; the large
            // picture takes the rest.
            let row = row(strip)
                .spacing(TILE_SPACING)
                .height(Length::Fixed(STRIP_HEIGHT));
            ground(
                column![
                    container(large).width(Length::Fill).height(Length::Fill),
                    container(row).width(Length::Fill).padding(
                        Padding::ZERO
                            .left(TILE_SPACING)
                            .right(TILE_SPACING)
                            .bottom(TILE_SPACING),
                    ),
                ]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
                tokens,
            )
        }
        StageLayout::Grid { columns, .. } => {
            let mut rows: Vec<Element<'a, M>> = Vec::new();
            let mut cells: Vec<Element<'a, M>> = Vec::new();
            for tile in strip {
                cells.push(tile);
                if cells.len() == columns {
                    rows.push(grid_row(std::mem::take(&mut cells)));
                }
            }
            if !cells.is_empty() {
                rows.push(grid_row(cells));
            }
            ground(
                column(rows)
                    .spacing(TILE_SPACING)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(TILE_SPACING)
                    .into(),
                tokens,
            )
        }
    }
}

/// The stage's own surface, which is what the letterbox bars around every tile
/// are.
fn ground<'a, M: 'a>(content: Element<'a, M>, tokens: &'a ThemeTokens) -> Element<'a, M> {
    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::container::chat(tokens))
        .into()
}

fn grid_row<'a, M: 'a>(cells: Vec<Element<'a, M>>) -> Element<'a, M> {
    row(cells)
        .spacing(TILE_SPACING)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The watched share, which is never pressable: it is the large picture unless a
/// camera has been promoted over it, and the pick list is what changes sharer.
fn share_tile<'a, M: Clone + 'a>(stage: &StageView<'a>, tokens: &'a ThemeTokens) -> Element<'a, M> {
    labelled(
        picture(stage.picture, stage.seq, TileId::Share, tokens),
        Cow::Borrowed(stage.sharer),
        tokens,
    )
}

/// One camera, labelled and pressable into the large picture.
fn camera_tile<'a, M: Clone + 'a>(
    tile: StageTile<'a>,
    press: M,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    let drawn = labelled(
        picture(tile.picture, tile.seq, TileId::from(tile.id), tokens),
        tile.name,
        tokens,
    );
    mouse_area(drawn).on_press(press).into()
}

/// A picture with the member's name over its bottom-left corner.
fn labelled<'a, M: 'a>(
    body: Element<'a, M>,
    name: Cow<'a, str>,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    let label = container(
        text(name.into_owned())
            .size(TEXT_BADGE)
            .color(tokens.text_primary),
    )
    .padding([2.0, 6.0])
    .style(styles::container::popover(tokens));

    stack![
        container(body).width(Length::Fill).height(Length::Fill),
        container(label)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(TILE_SPACING)
            .align_x(Horizontal::Left)
            .align_y(Vertical::Bottom),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn waiting<'a, M: 'a>(tokens: &'a ThemeTokens) -> Element<'a, M> {
    container(
        text("Waiting for video…")
            .size(TEXT_BODY)
            .color(tokens.text_muted),
    )
    .center(Length::Fill)
    .into()
}

fn toolbar<'a, M: Clone + 'a>(
    stage: StageView<'a>,
    handlers: StageHandlers<M>,
    cameras_on_stage: usize,
    tokens: &'a ThemeTokens,
    popped: bool,
) -> Element<'a, M> {
    let StageHandlers {
        watch,
        stop,
        stop_cameras,
        pop_out,
        pop_in,
        fullscreen,
        feature: _,
        volume,
        volume_released,
    } = handlers;

    // With no share on the stage the cameras are what is being watched, and the
    // close button comes off all of them at once.
    let (icon, title, close) = if stage.sharing {
        (Icon::Screen, format!("Watching {}", stage.sharer), stop)
    } else {
        let title = match cameras_on_stage {
            1 => "Watching 1 camera".to_owned(),
            _ => format!("Watching {cameras_on_stage} cameras"),
        };
        (Icon::Image, title, stop_cameras)
    };

    let mut bar = row![
        glyph(icon, tokens.text_secondary),
        text(title).size(TEXT_BODY).color(tokens.text_primary),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    // In fullscreen the video is the whole window: only the controls that get the
    // viewer back out of it, and the volume, stay. With a single sharer there is
    // nothing to pick between, so the name above is the whole story.
    if !stage.fullscreen && stage.sharing && stage.sharers.len() > 1 {
        let options: Vec<Sharer> = stage
            .sharers
            .into_iter()
            .map(|(id, username)| Sharer { id, username })
            .collect();
        let selected = options
            .iter()
            .find(|sharer| sharer.id == stage.current)
            .cloned();

        bar = bar.push(
            pick_list(options, selected, move |sharer| watch(sharer.id))
                .text_size(TEXT_SECONDARY)
                .padding([4.0, 8.0])
                .style(styles::pick_list(tokens))
                .menu_style(styles::menu(tokens)),
        );
    }

    bar = bar.push(Space::new().width(Length::Fill));

    // Only a share carries audio, so the volume belongs to one.
    if stage.sharing && stage.has_audio {
        bar = bar.push(
            slider(0.0..=SHARE_VOLUME_MAX, stage.volume, volume)
                .on_release(volume_released)
                .step(0.05_f32)
                .width(VOLUME_WIDTH)
                .style(styles::slider(tokens)),
        );
        bar = bar.push(
            text(format!("{:.0}%", stage.volume * 100.0))
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }

    if !stage.fullscreen {
        if !stage.stats.is_empty() {
            bar = bar.push(
                text(stage.stats)
                    .size(TEXT_SECONDARY)
                    .color(tokens.text_muted),
            );
        }
        let (tip, press) = if popped {
            ("Pop in", pop_in)
        } else {
            ("Pop out", pop_out)
        };
        bar = bar.push(icon_button(Icon::Pin, tip, press, tokens));
    }

    bar = bar.push(icon_button(
        Icon::Expand,
        if stage.fullscreen {
            "Exit fullscreen"
        } else {
            "Fullscreen"
        },
        fullscreen,
        tokens,
    ));
    bar = bar.push(tooltip_of(
        button(glyph(Icon::Close, tokens.text_on_accent))
            .padding(ICON_PADDING)
            .style(styles::button::danger(tokens))
            .on_press(close),
        "Stop watching",
        tokens,
    ));

    container(bar)
        .width(Length::Fill)
        .padding([6.0, 10.0])
        .style(styles::container::elevated(tokens))
        .into()
}

/// One picture, letterboxed inside whatever box it is given. The letterbox bars
/// are this container's ground: the shader only paints inside the aspect-fitted
/// rectangle.
fn picture<'a, M: 'a>(
    decoded: Option<&'a Arc<Picture>>,
    seq: u64,
    tile: TileId,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    let body: Element<'a, M> = match decoded {
        Some(decoded) => shader(StageProgram {
            picture: Some(decoded.clone()),
            seq,
            tile,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        None => waiting(tokens),
    };

    container(body)
        .center(Length::Fill)
        .style(styles::container::chat(tokens))
        .into()
}

/// One icon, square, in one colour. [`crate::icons::icon`] is the same thing
/// bound to [`Message`]; the stage is generic over its caller's message type, so
/// it builds its own, as it does for the two below.
fn glyph<'a, M: 'a>(icon: Icon, color: Color) -> Element<'a, M> {
    svg(icon.handle())
        .width(ICON_SIZE)
        .height(ICON_SIZE)
        .style(styles::svg(color))
        .into()
}

fn icon_button<'a, M: Clone + 'a>(
    icon: Icon,
    tip: &str,
    press: M,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    tooltip_of(
        button(glyph(icon, tokens.text_secondary))
            .padding(ICON_PADDING)
            .style(styles::button::icon(tokens))
            .on_press(press),
        tip,
        tokens,
    )
}

fn tooltip_of<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    tip: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, M> {
    tooltip(
        content,
        container(
            text(tip.to_owned())
                .size(TEXT_SECONDARY)
                .color(tokens.text_primary),
        )
        .padding([4.0, 8.0])
        .style(styles::container::popover(tokens)),
        tooltip::Position::Bottom,
    )
    .into()
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

/// Which picture a tile draws, and the key its textures are cached under. Every
/// tile on the stage keeps a set of its own: one renderer draws all of them
/// through the single pipeline below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileId {
    Share,
    /// This client's own preview.
    Own,
    Peer(i64),
}

impl From<CameraTileId> for TileId {
    fn from(tile: CameraTileId) -> Self {
        match tile {
            CameraTileId::Own => TileId::Own,
            CameraTileId::Peer(user_id) => TileId::Peer(user_id),
        }
    }
}

/// One decoded picture on its way to the shader.
pub struct StageProgram {
    pub picture: Option<Arc<Picture>>,
    pub seq: u64,
    pub tile: TileId,
}

impl<M> shader::Program<M> for StageProgram {
    type State = ();
    type Primitive = StagePrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> StagePrimitive {
        StagePrimitive {
            picture: self.picture.clone(),
            seq: self.seq,
            tile: self.tile,
        }
    }
}

/// The picture as the renderer takes it.
pub struct StagePrimitive {
    pub picture: Option<Arc<Picture>>,
    pub seq: u64,
    pub tile: TileId,
}

impl fmt::Debug for StagePrimitive {
    /// Sizes only: a picture is somebody's screen or somebody's face.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = self
            .picture
            .as_ref()
            .map(|picture| (picture.width, picture.height));
        f.debug_struct("StagePrimitive")
            .field("tile", &self.tile)
            .field("size", &size)
            .field("seq", &self.seq)
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

        pipeline.clock += 1;
        let touched = pipeline.clock;
        let stale = pipeline.tiles.get(&self.tile).is_none_or(|cached| {
            cached.planes.width != picture.width || cached.planes.height != picture.height
        });
        if stale {
            pipeline.tiles.insert(
                self.tile,
                Cached {
                    planes: Planes::new(
                        device,
                        &pipeline.layout,
                        &pipeline.sampler,
                        picture.width,
                        picture.height,
                    ),
                    uploaded_seq: None,
                    fit: None,
                    touched,
                },
            );
            pipeline.evict(self.tile);
        }

        let Some(cached) = pipeline.tiles.get_mut(&self.tile) else {
            return;
        };
        cached.touched = touched;

        // The primitive is rebuilt on every view(); only a new decoded picture is
        // worth the three uploads.
        if cached.uploaded_seq != Some(self.seq) {
            let chroma_width = picture.width.div_ceil(2);
            let chroma_height = picture.height.div_ceil(2);

            upload(
                queue,
                &cached.planes.y,
                &picture.y,
                picture.y_stride,
                picture.width,
                picture.height,
            );
            upload(
                queue,
                &cached.planes.u,
                &picture.u,
                picture.uv_stride,
                chroma_width,
                chroma_height,
            );
            upload(
                queue,
                &cached.planes.v,
                &picture.v,
                picture.uv_stride,
                chroma_width,
                chroma_height,
            );

            cached.uploaded_seq = Some(self.seq);
        }

        // iced draws with the render pass's viewport already set to the widget in
        // physical pixels; the fit is the same rectangle, shrunk to the picture's
        // aspect ratio.
        cached.fit = Some(fit_rect(
            *bounds * viewport.scale_factor(),
            picture.width,
            picture.height,
        ));
    }

    fn draw(&self, pipeline: &StagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(cached) = pipeline.tiles.get(&self.tile) else {
            return false;
        };
        let Some(fit) = cached.fit else {
            return false;
        };
        if fit.width < 1.0 || fit.height < 1.0 {
            return false;
        }

        render_pass.set_pipeline(&pipeline.render);
        render_pass.set_bind_group(0, &cached.planes.bind_group, &[]);
        render_pass.set_viewport(fit.x, fit.y, fit.width, fit.height, 0.0, 1.0);
        render_pass.draw(0..3, 0..1);

        true
    }
}

/// The Y, U and V planes on the device, one set per tile per renderer engine:
/// several tiles are on screen at once and every window's renderer has a
/// pipeline of its own.
pub struct StagePipeline {
    render: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    tiles: HashMap<TileId, Cached>,
    /// Counts prepares, so the least recently drawn tile can be told from the
    /// rest without a clock of any other kind.
    clock: u64,
}

/// One tile's planes, what was last uploaded into them and where they are drawn.
struct Cached {
    planes: Planes,
    uploaded_seq: Option<u64>,
    fit: Option<Rectangle>,
    touched: u64,
}

impl StagePipeline {
    /// Drops the tile drawn longest ago once the cache is over its bound, never
    /// the one being prepared. A peer who left is the only thing that ever fills
    /// it up, and their planes are megabytes.
    fn evict(&mut self, keep: TileId) {
        while self.tiles.len() > MAX_CACHED_TILES {
            let Some(oldest) = self
                .tiles
                .iter()
                .filter(|(tile, _)| **tile != keep)
                .min_by_key(|(_, cached)| cached.touched)
                .map(|(tile, _)| *tile)
            else {
                return;
            };
            self.tiles.remove(&oldest);
        }
    }
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
            tiles: HashMap::new(),
            clock: 0,
        }
    }

    // trim() is left at its no-op default: dropping the planes between frames
    // would cost a full re-upload on the next one, and they go with the pipeline
    // when the window does. `evict` is what bounds the map instead.
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
    // than the row cannot happen for I420, and a short plane would be rejected by
    // wgpu's own validation, so both are dropped rather than drawn.
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

/// The largest `width:height` rectangle inside `area`, centred, in whole physical
/// pixels.
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

/// The stage as the chat column draws it, built from the application state.
pub fn in_chat<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    view(stage_view(main), handlers(), &app.tokens)
}

/// The stage as the popped-out window draws it.
pub fn popped_window<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    popped(stage_view(main), handlers(), &app.tokens)
}

/// What the stage reads out of the state.
fn stage_view(main: &MainState) -> StageView<'_> {
    let watch = &main.voice.watch;
    let sharing = watch.state.is_some();
    StageView {
        sharer: rules::sharer_name(&main.voice, &main.server),
        sharers: rules::sharer_list(&main.voice, main.member_id),
        current: watch.state.unwrap_or_default(),
        picture: watch.picture.as_ref(),
        seq: watch.seq,
        sharing,
        cameras: camera_tiles(main),
        featured: main.voice.cameras.featured(),
        has_audio: watch
            .state
            .and_then(|user_id| main.voice.sharing(user_id))
            .unwrap_or(false),
        // Every step of a drag reaches the mixer through this; only its release
        // reaches `config.share_volume` and the disk.
        volume: watch.volume,
        stats: if sharing {
            stats_line(main)
        } else {
            camera_stats_line(main)
        },
        popped: watch.popped.is_some(),
        fullscreen: watch.fullscreen.is_some(),
    }
}

/// The camera tiles in the order the stage draws them, each with the newest
/// picture decoded for it — this client's own preview included.
fn camera_tiles(main: &MainState) -> Vec<StageTile<'_>> {
    let voice = &main.voice;
    rules::camera_tile_list(
        voice.camera.active,
        &voice.cameras.watched(),
        voice.roster(),
    )
    .into_iter()
    .map(|(id, name)| {
        let (picture, seq) = match id {
            CameraTileId::Own => (voice.camera.preview.as_ref(), voice.camera.preview_seq),
            CameraTileId::Peer(user_id) => match voice.cameras.tiles.get(&user_id) {
                Some(tile) => (tile.picture.as_ref(), tile.seq),
                None => (None, 0),
            },
        };
        StageTile {
            id,
            name: Cow::Owned(name),
            picture,
            seq,
        }
    })
    .collect()
}

/// The picture's own size, the decoder's rate, and what the depacketizer took in:
/// the viewer measures no bitrate of its own.
fn stats_line(main: &MainState) -> String {
    let watch = &main.voice.watch;
    let (width, height) = watch
        .picture
        .as_ref()
        .map_or((0, 0), |picture| (picture.width, picture.height));
    let fps = watch.stats.map_or(0.0, |(decode_fps, _, _)| decode_fps);
    let kbps = watch.kbps;
    format!("{width}×{height} · {fps:.0} fps · {kbps} kbit/s")
}

/// What the cameras on the stage are decoding at, once there is no share to
/// describe instead. The slowest tile is the one a viewer notices, so that is
/// the rate worth showing.
fn camera_stats_line(main: &MainState) -> String {
    let slowest = main
        .voice
        .cameras
        .tiles
        .values()
        .filter_map(|tile| tile.stats.map(|(decode_fps, _, _)| decode_fps))
        .min_by(f32::total_cmp);
    match slowest {
        Some(fps) => format!("{fps:.0} fps"),
        None => String::new(),
    }
}

/// The messages the stage sends.
fn handlers() -> StageHandlers<Message> {
    StageHandlers {
        watch: |user_id| Message::Share(ShareMsg::Watch(user_id)),
        stop: Message::Share(ShareMsg::StopWatching),
        stop_cameras: Message::Camera(CameraMsg::StopWatchingAll),
        pop_out: Message::Share(ShareMsg::PopOut),
        pop_in: Message::Share(ShareMsg::PopIn),
        fullscreen: Message::Share(ShareMsg::ToggleFullscreen),
        feature: |tile| Message::Camera(CameraMsg::Feature(tile)),
        volume: |volume| Message::Share(ShareMsg::SetVolume(volume)),
        volume_released: Message::Share(ShareMsg::VolumeReleased),
    }
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

    /// A share on the stage, or a camera promoted over it, is always the large
    /// picture, and everything else is a row under it.
    #[test]
    fn a_featured_picture_puts_the_rest_in_a_row() {
        for tiles in 0..=5 {
            assert_eq!(
                stage_layout(true, tiles),
                StageLayout::Featured { strip: tiles },
                "{tiles} beside the large picture"
            );
        }
    }

    /// With nothing featured the cameras fill the stage: one alone, two side by
    /// side, and two columns past that.
    #[test]
    fn cameras_alone_fill_the_stage_as_a_grid() {
        assert_eq!(stage_layout(false, 0), StageLayout::Empty);
        assert_eq!(
            stage_layout(false, 1),
            StageLayout::Grid {
                rows: 1,
                columns: 1
            }
        );
        assert_eq!(
            stage_layout(false, 2),
            StageLayout::Grid {
                rows: 1,
                columns: 2
            }
        );
        assert_eq!(
            stage_layout(false, 3),
            StageLayout::Grid {
                rows: 2,
                columns: 2
            }
        );
        assert_eq!(
            stage_layout(false, 4),
            StageLayout::Grid {
                rows: 2,
                columns: 2
            }
        );
        // Four watched cameras and this client's own preview is the most there
        // can ever be.
        assert_eq!(
            stage_layout(false, 5),
            StageLayout::Grid {
                rows: 3,
                columns: 2
            }
        );
    }

    /// Every grid holds every tile it was given: a row short of a column still
    /// gets a row of its own.
    #[test]
    fn every_grid_has_room_for_its_tiles() {
        for tiles in 1..=5 {
            let StageLayout::Grid { rows, columns } = stage_layout(false, tiles) else {
                panic!("{tiles} tiles laid out without a grid");
            };
            assert!(
                rows * columns >= tiles,
                "{tiles} tiles into {rows}x{columns}"
            );
            assert!(
                (rows - 1) * columns < tiles,
                "{rows}x{columns} has an empty row for {tiles} tiles"
            );
        }
    }

    /// The share, the local preview and each peer keep their own textures: one
    /// renderer draws all of them through one pipeline.
    #[test]
    fn every_tile_is_its_own_cache_key() {
        assert_eq!(TileId::from(CameraTileId::Own), TileId::Own);
        assert_eq!(TileId::from(CameraTileId::Peer(9)), TileId::Peer(9));

        let keys = [TileId::Share, TileId::Own, TileId::Peer(4), TileId::Peer(9)];
        for (at, key) in keys.iter().enumerate() {
            for other in &keys[at + 1..] {
                assert_ne!(key, other);
            }
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

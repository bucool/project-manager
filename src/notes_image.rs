//! Image loading and painting from GPUI, with independently resizable layout.
use gpui::{prelude::*, *};

pub(super) struct ResizableImage {
    image: Img,
    size: Size<Pixels>,
}

impl ResizableImage {
    pub fn new(image: Img, size: Size<Pixels>) -> Self {
        Self {
            image: image.object_fit(ObjectFit::Fill),
            size,
        }
    }
}

impl IntoElement for ResizableImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ResizableImage {
    type RequestLayoutState = <Img as Element>::RequestLayoutState;
    type PrepaintState = <Img as Element>::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Keep GPUI's asynchronous loading, cache and image state, but omit its
        // intrinsic-ratio layout node. Taffy's leaf layout enforces at least
        // width / aspect_ratio height even after clamping to an explicit max.
        let (_, state) = self
            .image
            .request_layout(global_id, inspector_id, window, cx);
        let style = Style {
            size: self.size.map(|dimension| dimension.into()),
            flex_shrink: 0.,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), state)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.image
            .prepaint(global_id, inspector_id, bounds, state, window, cx)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.image
            .paint(global_id, inspector_id, bounds, state, hitbox, window, cx);
    }
}

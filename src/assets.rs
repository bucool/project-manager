use gpui::{AssetSource, Result, SharedString};
use gpui_component::Icon;
use std::borrow::Cow;

const PLAY: &str = "project-manager/icons/play.svg";
const REFRESH: &str = "project-manager/icons/refresh.svg";
const EDIT: &str = "project-manager/icons/edit.svg";
const TRASH: &str = "project-manager/icons/trash.svg";
const CHECK: &str = "icons/check.svg";
pub const APP_ICON: &str = "project-manager/app-icon.png";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            APP_ICON => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/app-icon.png"
            )))),
            PLAY => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/play.svg"
            )))),
            REFRESH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/refresh.svg"
            )))),
            EDIT => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/edit.svg"
            )))),
            TRASH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/trash.svg"
            )))),
            _ => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = gpui_component_assets::Assets.list(path)?;
        assets.extend(
            [PLAY, REFRESH, EDIT, TRASH, APP_ICON]
                .into_iter()
                .filter(|asset| asset.starts_with(path))
                .map(Into::into),
        );
        Ok(assets)
    }
}

pub enum ActionIcon {
    Play,
    Refresh,
    Edit,
    Trash,
    Check,
}

impl From<ActionIcon> for Icon {
    fn from(icon: ActionIcon) -> Self {
        Icon::default().path(match icon {
            ActionIcon::Play => PLAY,
            ActionIcon::Refresh => REFRESH,
            ActionIcon::Edit => EDIT,
            ActionIcon::Trash => TRASH,
            ActionIcon::Check => CHECK,
        })
    }
}

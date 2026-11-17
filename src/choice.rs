//! Choosing among the cameras of the computer, from the page (Windows).
//!
//! [`CameraSelection`] is shared by the engine, which changes it when the person picks another camera, and the
//! source factory, which reads it each time the camera is opened. The choice is kept in `camera.json` next to
//! the calibration, by the name of the camera as well as its number: Windows numbers the cameras in the order
//! it finds them, and unplugging or plugging the USB one can change the numbers. `--device` on the command line
//! beats the kept choice.

use crate::fsutil::write_atomic;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// One camera of the computer, as the page offers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraInfo {
    /// What `--device` takes: the camera number on Windows.
    pub id: String,
    /// The name Windows gives it ("Integrated Camera", "HD Pro Webcam C920").
    pub name: String,
}

/// What `camera.json` holds.
#[derive(Serialize, Deserialize)]
struct Saved {
    id: String,
    name: String,
}

type Lister = Box<dyn Fn() -> Result<Vec<CameraInfo>> + Send + Sync>;

/// The camera in use, and the way to list the cameras there are.
pub struct CameraSelection {
    device: Mutex<String>,
    file: Option<PathBuf>,
    lister: Lister,
}

impl std::fmt::Debug for CameraSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraSelection").field("device", &self.device()).field("file", &self.file).finish()
    }
}

impl CameraSelection {
    /// `device` is the camera used until another is chosen; `file` is where the choice is kept.
    pub fn new(device: String, file: Option<PathBuf>) -> CameraSelection {
        CameraSelection::with_lister(device, file, Box::new(crate::camera::system_cameras))
    }

    pub fn with_lister(device: String, file: Option<PathBuf>, lister: Lister) -> CameraSelection {
        CameraSelection { device: Mutex::new(device), file, lister }
    }

    /// What the camera source opens now.
    pub fn device(&self) -> String {
        self.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// The cameras that are there now.
    pub fn list(&self) -> Result<Vec<CameraInfo>> {
        (self.lister)()
    }

    /// The camera with this number, if there is one.
    pub fn find(&self, id: &str) -> Result<CameraInfo> {
        match self.list()?.into_iter().find(|c| c.id == id) {
            Some(camera) => Ok(camera),
            None => bail!("there is no camera {id}: look for the cameras again"),
        }
    }

    /// Uses this camera from now on, and keeps the choice for the next start.
    pub fn store(&self, camera: &CameraInfo) -> Result<()> {
        if let Some(file) = &self.file {
            let saved = Saved { id: camera.id.clone(), name: camera.name.clone() };
            write_atomic(file, &serde_json::to_vec_pretty(&saved)?).with_context(|| "cannot keep the choice of the camera")?;
        }
        *self.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = camera.id.clone();
        Ok(())
    }

    /// Uses no camera from now on, and keeps the choice. This is how the second camera is switched off: the first
    /// one always has a camera, so only the second selection is ever cleared.
    pub fn clear(&self) -> Result<()> {
        if let Some(file) = &self.file {
            let saved = Saved { id: String::new(), name: String::new() };
            write_atomic(file, &serde_json::to_vec_pretty(&saved)?).with_context(|| "cannot keep the choice of the camera")?;
        }
        self.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
        Ok(())
    }

    /// Goes back to the camera chosen last time, if it is still there (by its name: the numbers may have changed).
    /// Anything wrong (no file, no such camera any more) leaves the camera as it is. "No camera" was chosen last
    /// time: no camera.
    pub fn restore(&self) {
        let Some(file) = &self.file else { return };
        let Ok(bytes) = std::fs::read(file) else { return };
        let Ok(saved) = serde_json::from_slice::<Saved>(&bytes) else {
            eprintln!("camera: {} cannot be read; using camera {}", file.display(), self.device());
            return;
        };
        if saved.id.is_empty() {
            self.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
            return;
        }
        let Ok(cameras) = self.list() else { return };
        // The same number with the same name is the likeliest; two cameras of one model have one name.
        let found =
            cameras.iter().find(|c| c.id == saved.id && c.name == saved.name).or_else(|| cameras.iter().find(|c| c.name == saved.name));
        match found {
            Some(camera) => {
                eprintln!("camera: using {} ({}), chosen last time", camera.id, camera.name);
                *self.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = camera.id.clone();
            }
            None => eprintln!("camera: \"{}\", chosen last time, is not plugged in; using camera {}", saved.name, self.device()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, name: &str) -> CameraInfo {
        CameraInfo { id: id.into(), name: name.into() }
    }

    fn selection(cameras: Vec<CameraInfo>, file: Option<PathBuf>) -> CameraSelection {
        CameraSelection::with_lister("0".into(), file, Box::new(move || Ok(cameras.clone())))
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicochess-choice-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("camera.json")
    }

    #[test]
    fn a_camera_is_found_by_its_number_and_the_choice_is_used_and_kept() {
        let file = temp_file("store");
        let cameras = vec![camera("0", "Integrated Camera"), camera("1", "USB Webcam")];
        let selection = selection(cameras.clone(), Some(file.clone()));
        assert_eq!(selection.device(), "0");
        assert!(selection.find("2").is_err(), "no such camera");
        let usb = selection.find("1").unwrap();
        assert_eq!(usb, cameras[1]);
        selection.store(&usb).unwrap();
        assert_eq!(selection.device(), "1");
        assert!(std::fs::read_to_string(&file).unwrap().contains("USB Webcam"));
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn at_the_next_start_the_camera_is_found_again_by_its_name_even_when_the_numbers_changed() {
        let file = temp_file("restore");
        let before = selection(vec![camera("0", "Integrated Camera"), camera("1", "USB Webcam")], Some(file.clone()));
        before.store(&camera("1", "USB Webcam")).unwrap();

        // Same cameras: the same one.
        let same = selection(vec![camera("0", "Integrated Camera"), camera("1", "USB Webcam")], Some(file.clone()));
        same.restore();
        assert_eq!(same.device(), "1");
        // The numbers changed (the USB camera was plugged in first): the name wins.
        let swapped = selection(vec![camera("0", "USB Webcam"), camera("1", "Integrated Camera")], Some(file.clone()));
        swapped.restore();
        assert_eq!(swapped.device(), "0");
        // The USB camera is not plugged in: the camera given at start stays.
        let gone = selection(vec![camera("0", "Integrated Camera")], Some(file.clone()));
        gone.restore();
        assert_eq!(gone.device(), "0");
        // Nothing kept, or something unreadable kept: nothing changes, and nothing fails.
        let none = selection(vec![camera("0", "A"), camera("1", "B")], Some(file.with_file_name("none.json")));
        none.restore();
        assert_eq!(none.device(), "0");
        std::fs::write(&file, "{ not json").unwrap();
        let broken = selection(vec![camera("0", "A"), camera("1", "B")], Some(file.clone()));
        broken.restore();
        assert_eq!(broken.device(), "0");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn no_camera_can_be_chosen_and_is_kept_for_the_next_start() {
        let file = temp_file("none");
        let cameras = vec![camera("0", "Integrated Camera"), camera("1", "USB Webcam")];
        let second = selection(cameras.clone(), Some(file.clone()));
        second.store(&cameras[1]).unwrap();
        assert_eq!(second.device(), "1");
        second.clear().unwrap();
        assert_eq!(second.device(), "");

        // At the next start the camera given then is not used: "no camera" was chosen.
        let again = selection(cameras, Some(file.clone()));
        assert_eq!(again.device(), "0");
        again.restore();
        assert_eq!(again.device(), "");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn two_cameras_with_one_name_are_told_apart_by_their_number() {
        let file = temp_file("twins");
        let twins = vec![camera("0", "HD Webcam"), camera("1", "HD Webcam")];
        let first = selection(twins.clone(), Some(file.clone()));
        first.store(&twins[1]).unwrap();
        let again = selection(twins, Some(file.clone()));
        again.restore();
        assert_eq!(again.device(), "1");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }
}

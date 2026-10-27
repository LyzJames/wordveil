//! Ciphertext-only scratch areas. Their random keys are never wrapped or saved.
use crate::{Error,Result};
use std::{fs::{self,File,OpenOptions},path::{Path,PathBuf}};

pub(crate) struct Area { pub path:PathBuf, owner:Option<File> }
fn owner(path:&Path)->std::io::Result<File>{
    let mut options=OpenOptions::new();options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt;options.share_mode(0);}
    options.open(path.join("owner.lock"))
}
fn ordinary_tree(path:&Path)->bool{
    let Ok(meta)=fs::symlink_metadata(path) else{return false;};
    #[cfg(windows)] {use std::os::windows::fs::MetadataExt;if meta.file_attributes()&0x400!=0{return false;}}
    if meta.file_type().is_symlink(){return false;}
    if meta.is_dir(){let Ok(entries)=fs::read_dir(path)else{return false;};for entry in entries{let Ok(entry)=entry else{return false;};if !ordinary_tree(&entry.path()){return false;}}}
    true
}
pub(crate) fn sweep(root:&Path)->Result<()> {
    if !root.exists(){return Ok(());}
    if !ordinary_tree(root){return Err(Error::Denied);}
    for entry in fs::read_dir(root)?{
        let entry=entry?;let name=entry.file_name();let Some(name)=name.to_str()else{continue;};
        if name.len()!=32||!name.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase())||!entry.file_type()?.is_dir(){continue;}
        // A different running app instance still owns its key and directory.
        if let Ok(file)=owner(&entry.path()){drop(file);let _=fs::remove_dir_all(entry.path());}
    }Ok(())
}
impl Area {
    pub fn create(root:&Path)->Result<Self>{
        if root.exists()&&!ordinary_tree(root){return Err(Error::Denied);}
        fs::create_dir_all(root)?;let path=root.join(uuid::Uuid::new_v4().simple().to_string());fs::create_dir(&path)?;
        let file=owner(&path)?;Ok(Self{path,owner:Some(file)})
    }
}
impl Drop for Area {fn drop(&mut self){self.owner.take();if ordinary_tree(&self.path){let _=fs::remove_dir_all(&self.path);}}}

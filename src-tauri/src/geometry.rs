//! Pixel-center transforms for the independently verified UVDoc sampler.
use crate::{Error,Result};
use base64::{Engine,engine::general_purpose::STANDARD};
use serde::Deserialize;

#[derive(Clone,Copy,Debug)]
pub struct Point {pub x:f64,pub y:f64}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GridWire {
    version:String,input_size:[u32;2],output_size:[u32;2],grid_shape:[u32;3],grid_f32_base64:String,
    align_corners:bool,padding_mode:String,interpolation:String,direction:String,
}
pub struct BackwardGrid {input:[u32;2],output:[u32;2],grid:Vec<f32>}
impl BackwardGrid {
    pub fn from_worker(value:&serde_json::Value)->Result<Self> {
        let wire:GridWire=serde_json::from_value(value.clone())?;
        if wire.version!="uvdoc-backward-grid/1" || wire.grid_shape!=[2,45,31] || !wire.align_corners || wire.padding_mode!="zeros" || wire.interpolation!="bilinear" || wire.direction!="rectified_to_source" {return Err(Error::Contract);}
        for dimensions in [wire.input_size,wire.output_size] {
            if dimensions[0]<2 || dimensions[1]<2 || u64::from(dimensions[0])*u64::from(dimensions[1])>4_194_304 {return Err(Error::Contract);}
        }
        if wire.grid_f32_base64.len()!=14880 {return Err(Error::Contract);}
        let bytes=STANDARD.decode(wire.grid_f32_base64).map_err(|_|Error::Contract)?;
        if bytes.len()!=2*45*31*4 {return Err(Error::Contract);}
        let grid=bytes.chunks_exact(4).map(|chunk|f32::from_le_bytes(chunk.try_into().unwrap())).collect::<Vec<_>>();
        if grid.iter().any(|v|!v.is_finite()) {return Err(Error::Contract);}
        Ok(Self{input:wire.input_size,output:wire.output_size,grid})
    }
    /// None means the model sampled zero padding outside the source image.
    /// Such points cannot be presented as a valid source highlight.
    pub fn to_source(&self,point:Point)->Result<Option<Point>> {
        if !point.x.is_finite() || !point.y.is_finite() || point.x<0. || point.y<0. || point.x>f64::from(self.output[0]-1) || point.y>f64::from(self.output[1]-1) {return Err(Error::Contract);}
        let gx=point.x*30./f64::from(self.output[0]-1);let gy=point.y*44./f64::from(self.output[1]-1);
        let x0=(gx.floor() as usize).min(30);let y0=(gy.floor() as usize).min(44);
        let x1=(x0+1).min(30);let y1=(y0+1).min(44);let dx=gx-x0 as f64;let dy=gy-y0 as f64;
        let sample=|channel:usize| {
            let at=|x:usize,y:usize|f64::from(self.grid[channel*45*31+y*31+x]);
            at(x0,y0)*(1.-dx)*(1.-dy)+at(x1,y0)*dx*(1.-dy)+at(x0,y1)*(1.-dx)*dy+at(x1,y1)*dx*dy
        };
        let source=Point{x:(sample(0)+1.)*f64::from(self.input[0]-1)/2.,y:(sample(1)+1.)*f64::from(self.input[1]-1)/2.};
        if source.x<0. || source.y<0. || source.x>f64::from(self.input[0]-1) || source.y>f64::from(self.input[1]-1) {return Ok(None);}
        Ok(Some(source))
    }
}

/// Undo an explicitly recorded clockwise quarter-turn of the input image.
pub fn unrotate(point:Point,original:[u32;2],clockwise_turns:u8)->Result<Point> {
    if original[0]==0 || original[1]==0 || clockwise_turns>3 || !point.x.is_finite() || !point.y.is_finite() {return Err(Error::Contract);}
    let w=f64::from(original[0]-1);let h=f64::from(original[1]-1);
    let (rw,rh)=if clockwise_turns%2==0 {(w,h)} else {(h,w)};
    if point.x<0. || point.y<0. || point.x>rw || point.y>rh {return Err(Error::Contract);}
    Ok(match clockwise_turns {
        0=>point,
        1=>Point{x:point.y,y:h-point.x},
        2=>Point{x:w-point.x,y:h-point.y},
        _=>Point{x:w-point.y,y:point.x},
    })
}

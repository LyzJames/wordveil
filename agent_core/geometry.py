"""Fixed FP32 UVDoc inference with an explicit rectified-to-source map.

The installed graph only adds a fetch of the original coarse backward grid;
parameters and the original image output are unchanged. No graph editing occurs
in the worker. Coordinates use pixel centers 0..width-1 / 0..height-1.
"""
from pathlib import Path
import base64,io,json

class UVDocCpu:
    def __init__(self,artifact:Path,threads:int):
        import paddle
        metadata=json.loads((artifact/'geometry.json').read_text(encoding='utf-8'))
        expected={'version':'uvdoc-backward-grid/1','layout':'NCHW-XY','align_corners':True,'padding_mode':'zeros','interpolation':'bilinear','direction':'rectified_to_source','precision':'fp32','weights_changed':False}
        if any(metadata.get(key)!=value for key,value in expected.items()):raise ValueError('Unsupported UVDoc geometry contract')
        config=paddle.inference.Config(str(artifact/'inference.json'),str(artifact/'inference.pdiparams'))
        config.disable_gpu();config.disable_mkldnn();config.set_cpu_math_library_num_threads(threads)
        config.enable_new_ir(True);config.enable_new_executor();config.disable_glog_info();config.enable_memory_optim()
        self.predictor=paddle.inference.create_predictor(config)
        self.image_output=metadata['image_output'];self.grid_output=metadata['grid_output']
        if set(self.predictor.get_output_names())!={self.image_output,self.grid_output}:raise ValueError('Unexpected UVDoc outputs')

    def predict(self,rgb):
        import numpy as np
        from PIL import Image
        # PaddleX Normalize uses float32 multiplication by 1/255, rather
        # than division. Preserve the exact reference rounding before UVDoc.
        normalized=rgb[:,:,::-1].astype(np.float32)
        normalized *= 1.0/255.0
        x=normalized.transpose(2,0,1)[None].copy()
        handle=self.predictor.get_input_handle(self.predictor.get_input_names()[0]);handle.reshape(x.shape);handle.copy_from_cpu(x);self.predictor.run()
        output=self.predictor.get_output_handle(self.image_output).copy_to_cpu()
        grid=self.predictor.get_output_handle(self.grid_output).copy_to_cpu()
        height,width=rgb.shape[:2]
        if output.shape!=(1,3,height,width) or grid.shape!=(1,2,45,31) or not np.isfinite(output).all() or not np.isfinite(grid).all():raise ValueError('UVDoc numerical or geometry contract failed')
        rectified=(output[0].transpose(1,2,0)[:,:,::-1]*np.float32(255.)).astype('uint8')
        encoded=io.BytesIO();Image.fromarray(rectified).save(encoded,format='PNG')
        geometry={'version':'uvdoc-backward-grid/1','input_size':[width,height],'output_size':[width,height],'grid_shape':[2,45,31],'grid_f32_base64':base64.b64encode(grid[0].astype('<f4').tobytes()).decode('ascii'),'align_corners':True,'padding_mode':'zeros','interpolation':'bilinear','direction':'rectified_to_source'}
        return {'png_base64':base64.b64encode(encoded.getvalue()).decode('ascii'),'shape':[height,width,3],'geometry':geometry,'geometry_mapping_verified':True}

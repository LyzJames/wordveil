# PyInstaller onedir is required: AppContainer Job admits one process, so a
# self-extracting onefile launcher is unsuitable. All output paths are set by the
# build command under docs. Model weights are separate, integrity-locked resources.
from pathlib import Path
from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs, copy_metadata
from importlib.metadata import distributions

app=Path(SPECPATH)
datas=[]
binaries=[]
for package in ['transformers','paddlex','paddleocr','paddle']:
    datas+=collect_data_files(package,include_py_files=True,excludes=['**/tests/**','**/test/**','**/__pycache__/**'])
for package in ['paddle','pypdfium2_raw','soundfile','usearch']:
    binaries+=collect_dynamic_libs(package)
build_only={'pip','pytest','pyinstaller','pyinstaller-hooks-contrib','onnx','onnxruntime','bitsandbytes','accelerate','matplotlib','contourpy','cycler','fonttools','kiwisolver','pip-licenses','prettytable','iniconfig','pluggy'}
for distribution in distributions():
    name=distribution.metadata['Name']
    if name.lower().replace('_','-') not in build_only:
        datas+=copy_metadata(name)

a=Analysis([str(app/'agent_core/entry.py')],pathex=[str(app)],binaries=binaries,datas=datas,
    hiddenimports=['agent_core.worker','agent_core.inference','agent_core.geometry','pydantic','PIL.Image','pypdfium2','usearch.index','jieba','librosa','soundfile','bidi','bidi.algorithm','pyclipper','shapely','shapely.ops','cv2'],
    excludes=['tkinter','pytest','IPython','matplotlib','onnx','onnxruntime','bitsandbytes','accelerate','pip','tensorboard','notebook','jupyter'],
    noarchive=False)
pyz=PYZ(a.pure)
exe=EXE(pyz,a.scripts,[],exclude_binaries=True,name='lvjin-worker',debug=False,bootloader_ignore_signals=False,strip=False,upx=False,console=True,manifest=str(app/'agent_core/worker.manifest'))
coll=COLLECT(exe,a.binaries,a.datas,strip=False,upx=False,name='lvjin-worker')

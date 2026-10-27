# PyInstaller onedir is required: AppContainer Job admits one process, so a
# self-extracting onefile launcher is unsuitable. All output paths are set by the
# build command under docs. Model weights are separate, integrity-locked resources.
from pathlib import Path
from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs, copy_metadata
from importlib.metadata import distributions

app=Path(SPECPATH)
datas=[]
binaries=[]
for package in ['transformers','paddlex','paddleocr','paddle','docx']:
    datas+=collect_data_files(package,include_py_files=True,excludes=['**/tests/**','**/test/**','**/__pycache__/**'])
for package in ['paddle','pypdfium2_raw','usearch']:
    binaries+=collect_dynamic_libs(package)
# soundfile and librosa are not used (FFmpeg decodes all media; ASR gets PCM samples).
unused={'soundfile','librosa','langgraph','langchain_core','agent_core.orchestrator','agent_core.finance','agent_core.extract'}
build_only=unused|{'pip','pytest','pyinstaller','pyinstaller-hooks-contrib','onnx','onnxruntime','bitsandbytes','accelerate','matplotlib','contourpy','cycler','fonttools','kiwisolver','pip-licenses','prettytable','iniconfig','pluggy'}
for distribution in distributions():
    name=distribution.metadata['Name']
    if name.lower().replace('_','-') not in build_only and not name.lower().replace('_','-').startswith(('langgraph','langchain')):
        datas+=copy_metadata(name)

a=Analysis([str(app/'agent_core/entry.py')],pathex=[str(app)],binaries=binaries,datas=datas,
    hiddenimports=['agent_core.worker', 'agent_core.inference', 'agent_core.geometry', 'agent_core.contracts', 'agent_core.local_service', 'agent_core.parsers', 'agent_core.ocr_ops', 'agent_core.chunking', 'pydantic', 'PIL.Image', 'PIL.ImageOps', 'pypdfium2', 'pypdfium2.raw', 'usearch.index', 'jieba', 'bidi', 'bidi.algorithm', 'pyclipper', 'shapely', 'shapely.ops', 'cv2', 'tokenizers', 'docx', 'docx.table', 'docx.text.paragraph', 'openpyxl', 'openpyxl.reader.excel', 'lxml.etree', 'lxml._elementpath', 'et_xmlfile', 'agent_core.privacy_has'],
    excludes=['tkinter','pytest','IPython','matplotlib','onnx','onnxruntime','bitsandbytes','accelerate','pip','tensorboard','notebook','jupyter',*sorted(unused)],
    noarchive=False)
# Other packages' hooks copy the metadata of optional dependencies; the unused
# ones are not bundled, so their metadata is left out as well.
a.datas=[entry for entry in a.datas if entry[0].replace('\\','/').split('/')[0].split('-')[0].lower().replace('_','-') not in unused]
pyz=PYZ(a.pure)
exe=EXE(pyz,a.scripts,[],exclude_binaries=True,name='lvjin-worker',debug=False,bootloader_ignore_signals=False,strip=False,upx=False,console=True,manifest=str(app/'agent_core/worker.manifest'))
coll=COLLECT(exe,a.binaries,a.datas,strip=False,upx=False,name='lvjin-worker')

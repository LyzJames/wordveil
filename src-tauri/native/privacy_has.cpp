// Fixed llama.cpp b11901 C ABI adapter. Loaded only in the isolated model
// worker. It has no network, file-selection, model-download or tool API.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include "llama.h"
#include "ggml-backend.h"
#include <chrono>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

static std::string utf8(const wchar_t *value) {
    if (!value) return {};
    int size=WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,value,-1,nullptr,0,nullptr,nullptr);
    if (size<=0) return {};
    std::string out(static_cast<size_t>(size),'\0');
    if (!WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,value,-1,out.data(),size,nullptr,nullptr)) return {};
    out.pop_back();return out;
}

static void silence(ggml_log_level,const char *,void *) {}

struct Api {
    HMODULE llama=nullptr,ggml=nullptr,base=nullptr;
#define API_FIELD(name) decltype(&::name) name=nullptr;
    API_FIELD(llama_log_set) API_FIELD(llama_backend_init)
    API_FIELD(llama_model_default_params) API_FIELD(llama_model_load_from_file)
    API_FIELD(llama_model_free) API_FIELD(llama_model_get_vocab)
    API_FIELD(llama_context_default_params) API_FIELD(llama_init_from_model) API_FIELD(llama_free)
    API_FIELD(llama_tokenize) API_FIELD(llama_token_to_piece) API_FIELD(llama_vocab_is_eog)
    API_FIELD(llama_batch_get_one) API_FIELD(llama_decode)
    API_FIELD(llama_sampler_init_greedy) API_FIELD(llama_sampler_sample) API_FIELD(llama_sampler_free)
    API_FIELD(ggml_backend_load_all_from_path) API_FIELD(ggml_log_set)
#undef API_FIELD
    bool load(const std::wstring &dir) {
        const DWORD flags=LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32;
        base=LoadLibraryExW((dir+L"\\ggml-base.dll").c_str(),nullptr,flags);
        ggml=LoadLibraryExW((dir+L"\\ggml.dll").c_str(),nullptr,flags);
        llama=LoadLibraryExW((dir+L"\\llama.dll").c_str(),nullptr,flags);
        if (!base||!ggml||!llama) return false;
#define LOAD_FROM(module,name) name=reinterpret_cast<decltype(name)>(GetProcAddress(module,#name));if(!name)return false;
        LOAD_FROM(base,ggml_log_set)
        LOAD_FROM(ggml,ggml_backend_load_all_from_path)
        LOAD_FROM(llama,llama_log_set) LOAD_FROM(llama,llama_backend_init)
        LOAD_FROM(llama,llama_model_default_params) LOAD_FROM(llama,llama_model_load_from_file)
        LOAD_FROM(llama,llama_model_free) LOAD_FROM(llama,llama_model_get_vocab)
        LOAD_FROM(llama,llama_context_default_params) LOAD_FROM(llama,llama_init_from_model) LOAD_FROM(llama,llama_free)
        LOAD_FROM(llama,llama_tokenize) LOAD_FROM(llama,llama_token_to_piece) LOAD_FROM(llama,llama_vocab_is_eog)
        LOAD_FROM(llama,llama_batch_get_one) LOAD_FROM(llama,llama_decode)
        LOAD_FROM(llama,llama_sampler_init_greedy) LOAD_FROM(llama,llama_sampler_sample) LOAD_FROM(llama,llama_sampler_free)
#undef LOAD_FROM
        ggml_log_set(silence,nullptr);llama_log_set(silence,nullptr);
        // The installation inventory contains only CPU backends in this
        // directory. No RPC, GPU, common CLI or server library is shipped.
        ggml_backend_load_all_from_path(utf8(dir.c_str()).c_str());
        llama_backend_init();return true;
    }
};

struct Handle { Api api; llama_model *model=nullptr; int threads=4; ~Handle(){if(model)api.llama_model_free(model);} };

extern "C" __declspec(dllexport) void *has_open(const wchar_t *runtime_dir,const wchar_t *model_path,int threads) noexcept {
    try {
        if(!runtime_dir||!model_path||threads<1||threads>8) return nullptr;
        auto handle=std::make_unique<Handle>();
        if(!handle->api.load(runtime_dir)) return nullptr;
        auto params=handle->api.llama_model_default_params();params.n_gpu_layers=0;
        handle->model=handle->api.llama_model_load_from_file(utf8(model_path).c_str(),params);
        if(!handle->model) return nullptr;
        handle->threads=threads;return handle.release();
    } catch(...) {return nullptr;}
}

extern "C" __declspec(dllexport) void has_close(void *opaque) noexcept {delete static_cast<Handle *>(opaque);}

static bool timed_out(void *opaque) {
    return std::chrono::steady_clock::now()>=*static_cast<std::chrono::steady_clock::time_point *>(opaque);
}

extern "C" __declspec(dllexport) int has_infer(void *opaque,const char *prompt,int prompt_bytes,int max_tokens,char *output,int output_capacity) noexcept {
    try {
        auto *handle=static_cast<Handle *>(opaque);
        if(!handle||!prompt||!output||prompt_bytes<1||prompt_bytes>65536||max_tokens<16||max_tokens>2048||output_capacity<2||output_capacity>262144) return -1;
        auto &api=handle->api;const auto *vocab=api.llama_model_get_vocab(handle->model);
        int count=api.llama_tokenize(vocab,prompt,prompt_bytes,nullptr,0,true,true);
        count=count<0?-count:count;
        if(count<1||count+max_tokens>8192) return -2;
        std::vector<llama_token> tokens(static_cast<size_t>(count));
        if(api.llama_tokenize(vocab,prompt,prompt_bytes,tokens.data(),count,true,true)!=count) return -3;
        auto deadline=std::chrono::steady_clock::now()+std::chrono::minutes(4);
        auto params=api.llama_context_default_params();
        params.n_ctx=8192;params.n_batch=512;params.n_ubatch=512;
        params.n_threads=handle->threads;params.n_threads_batch=handle->threads;
        params.no_perf=true;params.abort_callback=timed_out;params.abort_callback_data=&deadline;
        std::unique_ptr<llama_context,decltype(api.llama_free)> context(api.llama_init_from_model(handle->model,params),api.llama_free);
        if(!context) return -3;
        std::unique_ptr<llama_sampler,decltype(api.llama_sampler_free)> sampler(api.llama_sampler_init_greedy(),api.llama_sampler_free);
        if(!sampler) return -3;
        for(int start=0;start<count;start+=512) {
            const int batch_count=(count-start)<512?(count-start):512;
            if(api.llama_decode(context.get(),api.llama_batch_get_one(tokens.data()+start,batch_count))) return -3;
        }
        std::string result;
        for(int generated=0;generated<max_tokens;++generated) {
            if(timed_out(&deadline)) return -3;
            llama_token token=api.llama_sampler_sample(sampler.get(),context.get(),-1);
            if(api.llama_vocab_is_eog(vocab,token)) {
                if(result.size()+1>static_cast<size_t>(output_capacity)) return -4;
                std::memcpy(output,result.data(),result.size());output[result.size()]='\0';
                return static_cast<int>(result.size());
            }
            std::vector<char> piece(256);
            int bytes=api.llama_token_to_piece(vocab,token,piece.data(),static_cast<int>(piece.size()),0,false);
            if(bytes<0&&bytes>=-8192) {piece.resize(static_cast<size_t>(-bytes));bytes=api.llama_token_to_piece(vocab,token,piece.data(),static_cast<int>(piece.size()),0,false);}
            if(bytes<0) return -3;
            if(result.size()+static_cast<size_t>(bytes)+1>static_cast<size_t>(output_capacity)) return -4;
            result.append(piece.data(),static_cast<size_t>(bytes));
            if(api.llama_decode(context.get(),api.llama_batch_get_one(&token,1))) return -3;
        }
        return -4; // An output cap is not successful entity coverage.
    } catch(...) {return -3;}
}

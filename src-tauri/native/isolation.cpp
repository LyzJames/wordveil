// Windows security shim; all policy, paths and key management belong to Rust.
// API basis: Microsoft "Launch an AppContainer", STARTUPINFOEX and Job Objects.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <userenv.h>
#include <aclapi.h>
#include <sddl.h>
#include <string>
#include <vector>

extern "C" DWORD lvjin_profile_create(const wchar_t* name, void** sid) {
    HRESULT result=CreateAppContainerProfile(name,name,L"Lvjin isolated CPU worker",nullptr,0,reinterpret_cast<PSID*>(sid));
    if(result==HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS))
        result=DeriveAppContainerSidFromAppContainerName(name,reinterpret_cast<PSID*>(sid));
    return SUCCEEDED(result)?ERROR_SUCCESS:static_cast<DWORD>(result);
}

extern "C" void lvjin_profile_free(void* sid) { if(sid) FreeSid(sid); }
extern "C" DWORD lvjin_profile_delete(const wchar_t* name) {
    HRESULT result=DeleteAppContainerProfile(name);
    return SUCCEEDED(result)?ERROR_SUCCESS:static_cast<DWORD>(result);
}

extern "C" DWORD lvjin_grant_path(const wchar_t* path,void* sid,int writable) {
    PACL old_acl=nullptr,new_acl=nullptr;
    PSECURITY_DESCRIPTOR descriptor=nullptr;
    DWORD result=GetNamedSecurityInfoW(path,SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&old_acl,nullptr,&descriptor);
    if(result!=ERROR_SUCCESS) return result;
    EXPLICIT_ACCESSW access[2]{};
    access[0].grfAccessPermissions=FILE_GENERIC_READ|FILE_GENERIC_EXECUTE|(writable?FILE_GENERIC_WRITE|DELETE:0);
    access[0].grfAccessMode=GRANT_ACCESS;
    access[0].grfInheritance=SUB_CONTAINERS_AND_OBJECTS_INHERIT;
    access[0].Trustee.TrusteeForm=TRUSTEE_IS_SID;
    access[0].Trustee.TrusteeType=TRUSTEE_IS_UNKNOWN;
    access[0].Trustee.ptstrName=reinterpret_cast<LPWSTR>(sid);
    // Newly created work directories may inherit Modify for their owner, which
    // omits WRITE_OWNER required for LABEL_SECURITY_INFORMATION. Their owner can
    // adjust this private directory's DACL without elevating or changing E:\ ACLs.
    HANDLE token=nullptr; std::vector<unsigned char> user;
    if(writable) {
        if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&token)) { LocalFree(descriptor); return GetLastError(); }
        DWORD bytes=0; GetTokenInformation(token,TokenUser,nullptr,0,&bytes); user.resize(bytes);
        if(!GetTokenInformation(token,TokenUser,user.data(),bytes,&bytes)) { result=GetLastError(); CloseHandle(token); LocalFree(descriptor); return result; }
        CloseHandle(token);
        access[1].grfAccessPermissions=FILE_ALL_ACCESS;
        access[1].grfAccessMode=GRANT_ACCESS;
        access[1].grfInheritance=SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        access[1].Trustee.TrusteeForm=TRUSTEE_IS_SID;
        access[1].Trustee.TrusteeType=TRUSTEE_IS_USER;
        access[1].Trustee.ptstrName=reinterpret_cast<LPWSTR>(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid);
    }
    result=SetEntriesInAclW(writable?2:1,access,old_acl,&new_acl);
    if(result==ERROR_SUCCESS)
        result=SetNamedSecurityInfoW(const_cast<LPWSTR>(path),SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,new_acl,nullptr);
    if(new_acl) LocalFree(new_acl);
    if(descriptor) LocalFree(descriptor);
    if(result!=ERROR_SUCCESS || !writable) return result;
    PSECURITY_DESCRIPTOR label=nullptr;
    if(!ConvertStringSecurityDescriptorToSecurityDescriptorW(L"S:(ML;OICI;NW;;;LW)",SDDL_REVISION_1,&label,nullptr)) return GetLastError();
    PACL sacl=nullptr;
    BOOL present=FALSE,defaulted=FALSE;
    if(!GetSecurityDescriptorSacl(label,&present,&sacl,&defaulted)) result=GetLastError();
    else result=SetNamedSecurityInfoW(const_cast<LPWSTR>(path),SE_FILE_OBJECT,LABEL_SECURITY_INFORMATION,nullptr,nullptr,nullptr,sacl);
    LocalFree(label);
    return result;
}

// Custom capability for the application's own read-only runtime (worker
// bundle, model artifacts, FFmpeg). Granted once on those directories, so
// starting a worker no longer rewrites the ACL of thousands of files.
extern "C" DWORD lvjin_capability_sid(const wchar_t* name,void** out) {
    *out=nullptr;
    PSID* groups=nullptr; DWORD group_count=0; PSID* capabilities=nullptr; DWORD capability_count=0;
    if(!DeriveCapabilitySidsFromName(name,&groups,&group_count,&capabilities,&capability_count)) return GetLastError();
    DWORD result=ERROR_INVALID_DATA;
    if(capability_count>=1) {
        DWORD length=GetLengthSid(capabilities[0]);
        void* copy=LocalAlloc(LPTR,length);
        result=(copy && CopySid(length,copy,capabilities[0]))?ERROR_SUCCESS:GetLastError();
        if(result==ERROR_SUCCESS) *out=copy; else if(copy) LocalFree(copy);
    }
    for(DWORD i=0;i<group_count;i++) LocalFree(groups[i]);
    LocalFree(groups);
    for(DWORD i=0;i<capability_count;i++) LocalFree(capabilities[i]);
    LocalFree(capabilities);
    return result;
}

static bool is_package_sid(PSID sid) {
    // AppContainer SIDs are S-1-15-2-x1..x7; the well-known S-1-15-2-1/-2 have two sub-authorities.
    SID_IDENTIFIER_AUTHORITY package=SECURITY_APP_PACKAGE_AUTHORITY;
    if(!IsValidSid(sid) || memcmp(GetSidIdentifierAuthority(sid),&package,sizeof(package))!=0) return false;
    return *GetSidSubAuthorityCount(sid)>2 && *GetSidSubAuthority(sid,0)==SECURITY_APP_PACKAGE_BASE_RID;
}

// Idempotent: adds an inheritable read/execute ACE for the capability unless
// present, and removes explicit grants to individual AppContainers (left on
// these directories by per-process grants of earlier versions after a crash).
extern "C" DWORD lvjin_grant_runtime(const wchar_t* path,void* capability) {
    PACL old_acl=nullptr; PSECURITY_DESCRIPTOR descriptor=nullptr;
    DWORD result=GetNamedSecurityInfoW(path,SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&old_acl,nullptr,&descriptor);
    if(result!=ERROR_SUCCESS) return result;
    const ACCESS_MASK wanted=FILE_GENERIC_READ|FILE_GENERIC_EXECUTE;
    const BYTE inherit=OBJECT_INHERIT_ACE|CONTAINER_INHERIT_ACE;
    bool present=false;
    std::vector<PSID> stale;
    ACL_SIZE_INFORMATION size{};
    if(old_acl && GetAclInformation(old_acl,&size,sizeof(size),AclSizeInformation)) {
        for(DWORD index=0;index<size.AceCount;index++) {
            void* ace=nullptr;
            if(!GetAce(old_acl,index,&ace)) continue;
            auto header=reinterpret_cast<ACE_HEADER*>(ace);
            if(header->AceType!=ACCESS_ALLOWED_ACE_TYPE || (header->AceFlags&INHERITED_ACE)) continue;
            auto allowed=reinterpret_cast<ACCESS_ALLOWED_ACE*>(ace);
            PSID sid=reinterpret_cast<PSID>(&allowed->SidStart);
            if(EqualSid(sid,capability) && (allowed->Mask&wanted)==wanted && (header->AceFlags&inherit)==inherit) present=true;
            else if(is_package_sid(sid)) stale.push_back(sid);
        }
    }
    if(present && stale.empty()) { LocalFree(descriptor); return ERROR_SUCCESS; }
    std::vector<EXPLICIT_ACCESSW> entries;
    for(PSID sid:stale) {
        EXPLICIT_ACCESSW entry{};
        entry.grfAccessMode=REVOKE_ACCESS;
        entry.Trustee.TrusteeForm=TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType=TRUSTEE_IS_UNKNOWN;
        entry.Trustee.ptstrName=reinterpret_cast<LPWSTR>(sid);
        entries.push_back(entry);
    }
    if(!present) {
        EXPLICIT_ACCESSW entry{};
        entry.grfAccessPermissions=wanted;
        entry.grfAccessMode=GRANT_ACCESS;
        entry.grfInheritance=SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        entry.Trustee.TrusteeForm=TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType=TRUSTEE_IS_WELL_KNOWN_GROUP;
        entry.Trustee.ptstrName=reinterpret_cast<LPWSTR>(capability);
        entries.push_back(entry);
    }
    PACL new_acl=nullptr;
    result=SetEntriesInAclW(static_cast<ULONG>(entries.size()),entries.data(),old_acl,&new_acl);
    if(result==ERROR_SUCCESS) result=SetNamedSecurityInfoW(const_cast<LPWSTR>(path),SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,new_acl,nullptr);
    if(new_acl) LocalFree(new_acl);
    LocalFree(descriptor);
    return result;
}

extern "C" void lvjin_sid_free(void* sid) { if(sid) LocalFree(sid); }

// Profiles whose moniker starts with `prefix` (per-process worker profiles
// a crash or a killed test left behind): registry mapping and folder.
extern "C" DWORD lvjin_cleanup_profiles(const wchar_t* prefix,DWORD* removed) {
    *removed=0;
    HKEY key=nullptr;
    LSTATUS status=RegOpenKeyExW(HKEY_CURRENT_USER,L"Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppContainer\\Mappings",0,KEY_READ,&key);
    if(status==ERROR_FILE_NOT_FOUND) return ERROR_SUCCESS;
    if(status!=ERROR_SUCCESS) return static_cast<DWORD>(status);
    std::vector<std::wstring> monikers;
    const size_t prefix_length=wcslen(prefix);
    for(DWORD index=0;;index++) {
        wchar_t name[256]; DWORD length=256;
        status=RegEnumKeyExW(key,index,name,&length,nullptr,nullptr,nullptr,nullptr);
        if(status==ERROR_NO_MORE_ITEMS) break;
        if(status!=ERROR_SUCCESS) continue;
        wchar_t moniker[256]; DWORD bytes=sizeof(moniker);
        if(RegGetValueW(key,name,L"Moniker",RRF_RT_REG_SZ,nullptr,moniker,&bytes)==ERROR_SUCCESS && _wcsnicmp(moniker,prefix,prefix_length)==0) monikers.emplace_back(moniker);
    }
    RegCloseKey(key);
    for(const auto& moniker:monikers) if(SUCCEEDED(DeleteAppContainerProfile(moniker.c_str()))) (*removed)++;
    return ERROR_SUCCESS;
}

extern "C" DWORD lvjin_revoke_path(const wchar_t* path,void* sid) {
    PACL old_acl=nullptr,new_acl=nullptr;
    PSECURITY_DESCRIPTOR descriptor=nullptr;
    DWORD result=GetNamedSecurityInfoW(path,SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&old_acl,nullptr,&descriptor);
    if(result!=ERROR_SUCCESS) return result;
    EXPLICIT_ACCESSW access{};
    access.grfAccessMode=REVOKE_ACCESS;
    access.grfInheritance=SUB_CONTAINERS_AND_OBJECTS_INHERIT;
    access.Trustee.TrusteeForm=TRUSTEE_IS_SID;
    access.Trustee.TrusteeType=TRUSTEE_IS_UNKNOWN;
    access.Trustee.ptstrName=reinterpret_cast<LPWSTR>(sid);
    result=SetEntriesInAclW(1,&access,old_acl,&new_acl);
    if(result==ERROR_SUCCESS) result=SetNamedSecurityInfoW(const_cast<LPWSTR>(path),SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,new_acl,nullptr);
    if(new_acl) LocalFree(new_acl);
    if(descriptor) LocalFree(descriptor);
    return result;
}

extern "C" DWORD lvjin_spawn_container(
    const wchar_t* executable,wchar_t* command,const wchar_t* working,
    wchar_t* environment,void* sid,void* runtime_capability,HANDLE input,HANDLE output,HANDLE error,
    SIZE_T memory_limit,HANDLE* process_out,HANDLE* job_out,DWORD* pid_out) {
    *process_out=nullptr; *job_out=nullptr; *pid_out=0;
    SIZE_T size=0;
    InitializeProcThreadAttributeList(nullptr,2,0,&size);
    std::vector<unsigned char> storage(size);
    auto attributes=reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    if(!InitializeProcThreadAttributeList(attributes,2,0,&size)) return GetLastError();
    SECURITY_CAPABILITIES capabilities{};
    capabilities.AppContainerSid=sid;
    // No network, registry, credential, device or broad-filesystem
    // capabilities; only the application's own read-only runtime capability.
    SID_AND_ATTRIBUTES runtime{};
    if(runtime_capability) {
        runtime.Sid=runtime_capability;
        runtime.Attributes=SE_GROUP_ENABLED;
        capabilities.Capabilities=&runtime;
        capabilities.CapabilityCount=1;
    }
    HANDLE handles[]={input,output,error};
    DWORD result=ERROR_SUCCESS;
    if(!UpdateProcThreadAttribute(attributes,0,PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,&capabilities,sizeof(capabilities),nullptr,nullptr)
       || !UpdateProcThreadAttribute(attributes,0,PROC_THREAD_ATTRIBUTE_HANDLE_LIST,handles,sizeof(handles),nullptr,nullptr)) {
        result=GetLastError(); DeleteProcThreadAttributeList(attributes); return result;
    }
    HANDLE job=CreateJobObjectW(nullptr,nullptr);
    if(!job) { result=GetLastError(); DeleteProcThreadAttributeList(attributes); return result; }
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
    limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE|JOB_OBJECT_LIMIT_ACTIVE_PROCESS|JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.BasicLimitInformation.ActiveProcessLimit=1;
    limits.ProcessMemoryLimit=memory_limit;
    if(!SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limits,sizeof(limits))) {
        result=GetLastError(); CloseHandle(job); DeleteProcThreadAttributeList(attributes); return result;
    }
    JOBOBJECT_BASIC_UI_RESTRICTIONS ui{};
    ui.UIRestrictionsClass=JOB_OBJECT_UILIMIT_HANDLES|JOB_OBJECT_UILIMIT_GLOBALATOMS|
        JOB_OBJECT_UILIMIT_READCLIPBOARD|JOB_OBJECT_UILIMIT_WRITECLIPBOARD|
        JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS|JOB_OBJECT_UILIMIT_DISPLAYSETTINGS|
        JOB_OBJECT_UILIMIT_DESKTOP|JOB_OBJECT_UILIMIT_EXITWINDOWS;
    if(!SetInformationJobObject(job,JobObjectBasicUIRestrictions,&ui,sizeof(ui))) {
        result=GetLastError(); CloseHandle(job); DeleteProcThreadAttributeList(attributes); return result;
    }
    STARTUPINFOEXW startup{};
    wchar_t desktop[]=L"winsta0\\default";
    startup.StartupInfo.cb=sizeof(startup);
    startup.StartupInfo.lpDesktop=desktop;
    startup.StartupInfo.dwFlags=STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput=input;
    startup.StartupInfo.hStdOutput=output;
    startup.StartupInfo.hStdError=error;
    startup.lpAttributeList=attributes;
    PROCESS_INFORMATION process{};
    BOOL created=CreateProcessW(executable,command,nullptr,nullptr,TRUE,
        CREATE_SUSPENDED|CREATE_NO_WINDOW|CREATE_UNICODE_ENVIRONMENT|EXTENDED_STARTUPINFO_PRESENT,
        environment,working,&startup.StartupInfo,&process);
    result=created?ERROR_SUCCESS:GetLastError();
    DeleteProcThreadAttributeList(attributes);
    if(!created) { CloseHandle(job); return result; }
    if(!AssignProcessToJobObject(job,process.hProcess) || ResumeThread(process.hThread)==static_cast<DWORD>(-1)) {
        result=GetLastError(); TerminateProcess(process.hProcess,1); WaitForSingleObject(process.hProcess,5000);
        CloseHandle(process.hThread); CloseHandle(process.hProcess); CloseHandle(job); return result;
    }
    CloseHandle(process.hThread);
    *process_out=process.hProcess; *job_out=job; *pid_out=process.dwProcessId;
    return ERROR_SUCCESS;
}

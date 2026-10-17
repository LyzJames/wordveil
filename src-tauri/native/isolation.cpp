// Windows security shim; all policy, paths and key management belong to Rust.
// API basis: Microsoft "Launch an AppContainer", STARTUPINFOEX and Job Objects.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <userenv.h>
#include <aclapi.h>
#include <sddl.h>
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
    wchar_t* environment,void* sid,HANDLE input,HANDLE output,HANDLE error,
    SIZE_T memory_limit,HANDLE* process_out,HANDLE* job_out,DWORD* pid_out) {
    *process_out=nullptr; *job_out=nullptr; *pid_out=0;
    SIZE_T size=0;
    InitializeProcThreadAttributeList(nullptr,2,0,&size);
    std::vector<unsigned char> storage(size);
    auto attributes=reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    if(!InitializeProcThreadAttributeList(attributes,2,0,&size)) return GetLastError();
    SECURITY_CAPABILITIES capabilities{};
    capabilities.AppContainerSid=sid;
    // No network, registry, credential, device or broad-filesystem capabilities.
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

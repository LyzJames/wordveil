import {invoke} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import type {Progress} from './api';
export type Project={id:string;title:string;legacy:boolean;root:boolean;documents:number};
export type PersonalFile={id:string;name:string;project_id:string;kind:string;status:string;bytes:number;pages:number;error:string|null};
export type ConversationSummary={id:string;title:string;project_id:string|null;connection_id:string|null;media_mode:'local'|'cloud';updated_at:string};
export type Source={source_number:number;source_id:string;version_id:string|null;content:string;trust:string};
export type ChatMessage={id:string;role:'user'|'assistant';text:string;status:string;sources:Source[];created_at:string;ordinal:number};
export type ChatPreview={messages:{role:string;text:string}[];sources:Source[];warnings:string[];replacements:number;policy:string;raw_media:boolean};
export type ContextInfo={window:number;output_reserve:number;input_limit:number;estimated_input:number;history_messages:number;recent_messages:number;compacted_messages:number;memory_available:number;memory_ids:string[];sources:number;method:string};
export type Memory={id:string;text:string;project_id:string|null;project_title:string|null;source_conversation:string|null;source_message:string|null;updated_at:string};
export type Conversation={id:string;title:string;project_id:string|null;media_mode:'local'|'cloud';messages:ChatMessage[];pending:{id:string;state:string;preview:ChatPreview|null}|null;branch:string;revision:number;branches:{id:string;title:string}[];message_count:number;temporary:boolean;context:ContextInfo|null};
export type SearchResult={items:{source_id:string;version_id:string;name:string;text:string}[];strategy:string;truncated:boolean;warning?:string};
export type ChatRequest={conversation_id:string;text:string;connection_id:string;projects:string[];versions:string[];retrieval:'auto'|'exact'|'semantic';agentic:boolean;web:boolean;search_connection_id:string|null;media_mode:'local'|'cloud';effort:'auto'|'low'|'medium'|'high';continue_from?:string|null};
export type PersonalEvent={task_id:string;kind:string;progress:Progress|null;envelope:{status?:string;turn_id?:string;conversation_id?:string;text?:string;error?:string;message?:string;warnings?:string[];partial?:boolean}|null};
async function query<T>(operation:string,request:object={}):Promise<T>{try{return await invoke<T>('personal_query',{operation,request});}catch(e){throw new Error(String(e));}}
export const personal={
 action:(operation:string,request:object={})=>invoke<{id?:string;deleted?:boolean;status?:string;root_matter?:string}>('personal_action',{operation,request}),
 older:(id:string,before:number)=>query<ChatMessage[]>('older_messages',{id,before}),memories:()=>query<Memory[]>('memories'),context:(id:string)=>query<{items:{message_id:string;quote:string}[];notice:string}>('context',{id}),contextSettings:(connection_id:string,window?:number)=>query<{window:number;basis:string}>('context_settings',{connection_id,window}),
 conversations:(search='')=>query<ConversationSummary[]>('conversations',{search}),createConversation:(project_id:string|null)=>query<{id:string}>('create_conversation',{project_id}),conversation:(id:string)=>query<Conversation>('read_conversation',{id}),
 exportConversation:(id:string)=>query<{id:string}>('conversation_export',{id}),projects:()=>query<Project[]>('projects'),createProject:(title:string)=>query<{id:string}>('create_project',{title}),files:(project_id:string|null=null)=>query<PersonalFile[]>('files',{project_id}),
 privacy:()=>query<{yaml:string}>('privacy_config'),savePrivacy:(yaml:string)=>query('save_privacy_config',{yaml}),search:(request:{query:string;projects:string[];versions:string[];strategy:string})=>query<SearchResult>('search',request),discard:(id:string)=>query('discard_turn',{id}),
 send:(request:ChatRequest)=>invoke<{task_id?:string;status?:string}>('chat_send',{request}),approve:(turn:string)=>invoke<{task_id?:string;status?:string}>('chat_approve',{turn}),
};
export const onPersonal=(f:(e:PersonalEvent)=>void)=>listen<PersonalEvent>('lvjin://task',e=>f(e.payload));

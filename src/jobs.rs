//! A private, local job-search tracker.  It never fetches jobs or sends applications:
//! providers can hand it rows later, while imports and notifications remain explicit.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use crc32fast::Hasher;
use flate2::read::DeflateDecoder;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{Error, Result, Store};

pub const STAGES: &[&str] = &["saved", "draft", "applied", "interview", "offer", "rejected", "archived"];
const DEFAULT_SKILLS: &str = "react,next.js,node.js,typescript,python,mern";
const DEFAULT_LOCATIONS: &str = "kerala,bengaluru,dubai,gcc";
const DEFAULT_EXCLUDED: &str = "java,.net";

#[derive(Debug, Clone, Serialize)]
pub struct Preferences {
    pub skills: Vec<String>,
    pub locations: Vec<String>,
    pub excluded_requirements: Vec<String>,
    pub preferred_age_hours: i64,
    pub fallback_age_days: i64,
    pub notifications_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub company: String,
    pub title: String,
    pub location: String,
    pub url: Option<String>,
    pub source: String,
    pub posted_at: Option<String>,
    pub source_date_note: String,
    pub description: String,
    pub stage: String,
    pub notes: String,
    pub follow_up_on: Option<String>,
    pub match_score: i64,
    pub match_reasons: Vec<String>,
    pub notification_read: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub id: i64,
    pub job_id: i64,
    pub company: String,
    pub title: String,
    pub location: String,
    pub match_score: i64,
    pub message: String,
    pub created_at: String,
    pub read_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobInput {
    pub company: String,
    pub title: String,
    #[serde(default)] pub location: String,
    #[serde(default)] pub url: String,
    #[serde(default)] pub source: String,
    #[serde(default)] pub posted_at: String,
    #[serde(default)] pub source_date_note: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub stage: String,
    #[serde(default)] pub notes: String,
    #[serde(default)] pub follow_up_on: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobPatch {
    pub stage: Option<String>,
    pub notes: Option<String>,
    pub follow_up_on: Option<Option<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreferencesPatch {
    pub skills: Vec<String>,
    pub locations: Vec<String>,
    pub excluded_requirements: Vec<String>,
    pub preferred_age_hours: i64,
    pub fallback_age_days: i64,
    pub notifications_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct ImportResult { pub inserted: usize, pub duplicates: usize, pub invalid: usize }

#[derive(Debug, Serialize)]
pub struct JobsPage { pub jobs: Vec<Job>, pub unread: i64, pub preferences: Preferences }

fn list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(|v| v.to_ascii_lowercase()).collect()
}
fn joined(values: &[String]) -> String {
    values.iter().map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty()).collect::<Vec<_>>().join(",")
}
fn trim_required(value: &str, name: &str) -> Result<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() { Err(Error::Invalid(format!("{name} is required"))) } else if value.len() > 300 { Err(Error::Invalid(format!("{name} is too long"))) } else { Ok(value) }
}
fn day(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    if value.is_empty() { return Ok(None); }
    if value.len() == 10 && value.as_bytes().get(4) == Some(&b'-') && value.as_bytes().get(7) == Some(&b'-')
        && value.chars().enumerate().all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit()) { Ok(Some(value.into()))
    } else { Err(Error::Invalid("dates must use YYYY-MM-DD".into())) }
}
fn stage(value: &str) -> Result<String> {
    let value = if value.trim().is_empty() { "saved" } else { value.trim().to_ascii_lowercase().replace(' ', "-") };
    if STAGES.contains(&value.as_str()) { Ok(value) } else { Err(Error::Invalid(format!("unknown application stage {value:?}"))) }
}
fn normal_url(value: &str) -> Result<Option<String>> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() { return Ok(None); }
    if !(value.starts_with("https://") || value.starts_with("http://")) || value.len() > 2_000 { return Err(Error::Invalid("job URL must be an http(s) URL".into())); }
    Ok(Some(value.to_string()))
}
fn dedupe_key(company: &str, title: &str, location: &str, url: Option<&str>) -> String {
    match url { Some(url) => format!("url:{}", url.to_ascii_lowercase()), None => format!("role:{}|{}|{}", company.to_ascii_lowercase(), title.to_ascii_lowercase(), location.to_ascii_lowercase()) }
}
fn job_row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?, company: r.get(1)?, title: r.get(2)?, location: r.get(3)?, url: r.get(4)?, source: r.get(5)?, posted_at: r.get(6)?, source_date_note: r.get(7)?, description: r.get(8)?, stage: r.get(9)?, notes: r.get(10)?, follow_up_on: r.get(11)?, match_score: r.get(12)?,
        match_reasons: r.get::<_, String>(13)?.split('|').filter(|s| !s.is_empty()).map(String::from).collect(), notification_read: r.get::<_, i64>(14)? != 0, created_at: r.get(15)?, updated_at: r.get(16)?,
    })
}
const JOB_SELECT: &str = "SELECT j.id,j.company,j.title,j.location,j.url,j.source,j.posted_at,j.source_date_note,j.description,j.stage,j.notes,j.follow_up_on,j.match_score,j.match_reasons,NOT EXISTS(SELECT 1 FROM job_notifications n WHERE n.job_id=j.id AND n.read_at IS NULL),j.created_at,j.updated_at FROM jobs j";

impl Store {
    pub fn job_preferences(&self) -> Result<Preferences> {
        let values: (String, String, String, i64, i64, i64) = self.conn.query_row(
            "SELECT skills, locations, excluded_requirements, preferred_age_hours, fallback_age_days, notifications_enabled FROM job_preferences WHERE id=1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        Ok(Preferences { skills: list(&values.0), locations: list(&values.1), excluded_requirements: list(&values.2), preferred_age_hours: values.3, fallback_age_days: values.4, notifications_enabled: values.5 != 0 })
    }
    pub fn update_job_preferences(&self, patch: PreferencesPatch) -> Result<Preferences> {
        if !(1..=168).contains(&patch.preferred_age_hours) || !(1..=30).contains(&patch.fallback_age_days) { return Err(Error::Invalid("fresh and fallback windows must be reasonable positive values".into())); }
        self.write(|c| { c.execute("UPDATE job_preferences SET skills=?1,locations=?2,excluded_requirements=?3,preferred_age_hours=?4,fallback_age_days=?5,notifications_enabled=?6 WHERE id=1", params![joined(&patch.skills),joined(&patch.locations),joined(&patch.excluded_requirements),patch.preferred_age_hours,patch.fallback_age_days,patch.notifications_enabled])?; Ok(()) })?;
        self.job_preferences()
    }
    pub fn list_jobs(&self, query: &str, wanted_stage: Option<&str>) -> Result<Vec<Job>> {
        let mut sql = String::from(JOB_SELECT);
        let mut values = Vec::new();
        let q = query.trim().to_ascii_lowercase();
        if !q.is_empty() { sql.push_str(" WHERE lower(j.company||' '||j.title||' '||j.location||' '||j.description||' '||j.notes) LIKE ?1 ESCAPE '\\'"); values.push(format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))); }
        if let Some(wanted_stage) = wanted_stage.filter(|s| !s.is_empty() && *s != "all") { sql.push_str(if values.is_empty() { " WHERE j.stage=?1" } else { " AND j.stage=?2" }); values.push(stage(wanted_stage)?); }
        sql.push_str(" ORDER BY j.match_score DESC,j.posted_at DESC,j.updated_at DESC,j.id DESC");
        let mut statement = self.conn.prepare(&sql)?;
        Ok(statement.query_map(rusqlite::params_from_iter(values.iter()), job_row)?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn jobs_page(&self, query: &str, wanted_stage: Option<&str>) -> Result<JobsPage> {
        let unread = self.conn.query_row("SELECT count(*) FROM job_notifications WHERE read_at IS NULL", [], |r| r.get(0))?;
        Ok(JobsPage { jobs: self.list_jobs(query, wanted_stage)?, unread, preferences: self.job_preferences()? })
    }
    pub fn create_job(&self, input: &JobInput) -> Result<Job> {
        let company = trim_required(&input.company, "company")?; let title = trim_required(&input.title, "title")?;
        let location = input.location.trim().to_string(); let url = normal_url(&input.url)?; let posted_at = day(&input.posted_at)?; let follow_up_on = day(&input.follow_up_on)?; let stage = stage(&input.stage)?;
        let key = dedupe_key(&company, &title, &location, url.as_deref());
        let preferences = self.job_preferences()?; let (score, reasons) = match_job(&company, &title, &location, &input.description, &preferences);
        let id = self.write(|c| {
            if c.query_row("SELECT id FROM jobs WHERE dedupe_key=?1", [&key], |r| r.get::<_, i64>(0)).optional()?.is_some() { return Err(Error::Invalid("this job is already tracked (same URL or company, title and location)".into())); }
            c.execute("INSERT INTO jobs(company,title,location,url,source,posted_at,source_date_note,description,stage,notes,follow_up_on,dedupe_key,match_score,match_reasons) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", params![company,title,location,url,input.source.trim(),posted_at,input.source_date_note.trim(),input.description.trim(),stage,input.notes.trim(),follow_up_on,key,score,reasons.join("|")])?;
            let id=c.last_insert_rowid();
            if preferences.notifications_enabled && score > 0 && is_fresh(posted_at.as_deref(), preferences.preferred_age_hours, preferences.fallback_age_days) { c.execute("INSERT INTO job_notifications(job_id,message) VALUES(?1,?2)", params![id, format!("{} · {} matches your profile", company, title)])?; }
            Ok(id)
        })?;
        self.get_job(id)
    }
    pub fn get_job(&self, id: i64) -> Result<Job> {
        self.conn.prepare(&format!("{JOB_SELECT} WHERE j.id=?1"))?.query_row([id], job_row).optional()?.ok_or_else(|| Error::NotFound(format!("no job #{id}")))
    }
    pub fn update_job(&self, id: i64, patch: JobPatch) -> Result<Job> {
        if let Some(ref s) = patch.stage { let _=stage(s)?; }
        let follow = patch.follow_up_on.as_ref().map(|v| v.as_deref().map(day).transpose()).transpose()?;
        self.write(|c| { if let Some(s)=&patch.stage { c.execute("UPDATE jobs SET stage=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?2",params![stage(s)?,id])?; } if let Some(n)=&patch.notes { c.execute("UPDATE jobs SET notes=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?2",params![n.trim(),id])?; } if let Some(f)=&follow { c.execute("UPDATE jobs SET follow_up_on=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?2",params![f,id])?; } if c.changes()==0 && self.get_job(id).is_err() { return Err(Error::NotFound(format!("no job #{id}"))); } Ok(()) })?;
        self.get_job(id)
    }
    pub fn notifications(&self) -> Result<Vec<Notification>> {
        let mut s=self.conn.prepare("SELECT n.id,j.id,j.company,j.title,j.location,j.match_score,n.message,n.created_at,n.read_at FROM job_notifications n JOIN jobs j ON j.id=n.job_id ORDER BY n.read_at IS NOT NULL,n.created_at DESC")?;
        Ok(s.query_map([],|r| Ok(Notification{id:r.get(0)?,job_id:r.get(1)?,company:r.get(2)?,title:r.get(3)?,location:r.get(4)?,match_score:r.get(5)?,message:r.get(6)?,created_at:r.get(7)?,read_at:r.get(8)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn read_notifications(&self) -> Result<()> { self.write(|c| { c.execute("UPDATE job_notifications SET read_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE read_at IS NULL",[])?; Ok(()) }) }
    pub fn import_jobs(&self, rows: Vec<JobInput>) -> Result<ImportResult> { let mut out=ImportResult{inserted:0,duplicates:0,invalid:0}; for row in rows { match self.create_job(&row) { Ok(_)=>out.inserted+=1, Err(Error::Invalid(e)) if e.contains("already tracked")=>out.duplicates+=1, Err(_)=>out.invalid+=1, } } Ok(out) }
}

fn match_job(company:&str,title:&str,location:&str,description:&str,p:&Preferences)->(i64,Vec<String>){
 let text=format!("{} {} {} {}",company,title,location,description).to_ascii_lowercase(); let mut score=0; let mut reasons=Vec::new();
 let skills:Vec<_>=p.skills.iter().filter(|s| text.contains(s.as_str())).collect(); if skills.len()>=2 {score+=60;reasons.push(format!("{} relevant skills",skills.len()));} else if skills.len()==1 {score+=30;reasons.push(format!("{}",skills[0]));}
 if let Some(place)=p.locations.iter().find(|s| location.to_ascii_lowercase().contains(s.as_str())) {score+=25;reasons.push(place.to_string());}
 if p.excluded_requirements.iter().any(|s| text.contains(s.as_str())) {score-=100;reasons.push("excluded requirement mentioned".into());}
 (score.max(0),reasons)
}
fn is_fresh(posted:Option<&str>,hours:i64,days:i64)->bool { let Some(posted)=posted else{return false}; let query=format!("SELECT julianday('now')-julianday('{posted}')"); let c=rusqlite::Connection::open_in_memory(); c.ok().and_then(|c|c.query_row(&query,[],|r|r.get::<_,f64>(0)).ok()).is_some_and(|age|age>=0.0&&age<=days as f64&& (age*24.0<=hours as f64 || age<=days as f64)) }

/// Accepts CSV and ordinary .xlsx workbooks. Column names are matched liberally so
/// a personal tracker can be imported without exposing it to a third party.
pub fn parse_import(bytes:&[u8], name:&str)->std::result::Result<Vec<JobInput>,String>{
 let rows=if name.to_ascii_lowercase().ends_with(".xlsx"){xlsx_rows(bytes)?}else{csv_rows(std::str::from_utf8(bytes).map_err(|_|"CSV must be UTF-8")?)}; Ok(rows_to_jobs(rows))
}
fn csv_rows(text:&str)->Vec<Vec<String>> { let mut rows=Vec::new();let(mut row,mut cell,mut quoted)=(Vec::new(),String::new(),false);let mut chars=text.chars().peekable();while let Some(c)=chars.next(){match c{ '"' if quoted&&chars.peek()==Some(&'"')=>{cell.push('"');chars.next();},'"'=>quoted=!quoted,',' if !quoted=>{row.push(std::mem::take(&mut cell));},'\n' if !quoted=>{row.push(std::mem::take(&mut cell));rows.push(std::mem::take(&mut row));},'\r' if !quoted=>{},_=>cell.push(c)}}if !cell.is_empty()||!row.is_empty(){row.push(cell);rows.push(row)}rows }
fn rows_to_jobs(rows:Vec<Vec<String>>)->Vec<JobInput>{let Some(header)=rows.first()else{return Vec::new()};let mut cols=HashMap::new();for(i,h)in header.iter().enumerate(){cols.insert(h.to_ascii_lowercase().replace([' ','_','-'],""),i);}let get=|row:&Vec<String>,names:&[&str]|names.iter().find_map(|n|cols.get(*n).and_then(|i|row.get(*i))).cloned().unwrap_or_default();rows.into_iter().skip(1).filter_map(|r|{let company=get(&r,&["company","employer"]);let title=get(&r,&["title","jobtitle","role","position"]);(!company.trim().is_empty()&&!title.trim().is_empty()).then(||JobInput{company,title,location:get(&r,&["location","city"]),url:get(&r,&["url","link","applicationurl"]),source:get(&r,&["source","site"]),posted_at:get(&r,&["postedat","posteddate","date"]),source_date_note:get(&r,&["sourcedatenote","datecaveat","dateconfidence"]),description:get(&r,&["description","details","fitnotes"]),stage:get(&r,&["stage","status","applicationstage"]),notes:get(&r,&["notes","note"]),follow_up_on:get(&r,&["followupon","followup","nextfollowup"])})}).collect()}
fn xlsx_rows(bytes:&[u8])->std::result::Result<Vec<Vec<String>>,String>{let files=zip_files(bytes)?;let shared=files.get("xl/sharedStrings.xml").map(|b|xml_texts(b)).unwrap_or_default();let sheet=files.get("xl/worksheets/sheet1.xml").ok_or("workbook has no first worksheet")?;let xml=std::str::from_utf8(sheet).map_err(|_|"invalid worksheet XML")?;let mut rows=Vec::new();for r in xml.split("<row").skip(1){let Some(end)=r.find("</row>")else{continue};let mut values=Vec::new();for c in r[..end].split("<c ").skip(1){let head=c.split('>').next().unwrap_or("");let body=c.split_once('>').map(|x|x.1).unwrap_or("");let at=attr(head,"t");let raw=tag(body,"v").or_else(||tag(body,"t")).unwrap_or_default();let value=if at.as_deref()==Some("s"){raw.parse::<usize>().ok().and_then(|i|shared.get(i)).cloned().unwrap_or_default()}else{unescape(&raw)};let col=attr(head,"r").map(|r|r.chars().take_while(|c|c.is_ascii_alphabetic()).fold(0usize,|n,c|n*26+(c.to_ascii_uppercase()as usize-'A' as usize+1))).unwrap_or(values.len()+1);while values.len()<col{values.push(String::new())}values[col-1]=value;}rows.push(values)}Ok(rows)}
fn xml_texts(bytes:&[u8])->Vec<String>{let s=String::from_utf8_lossy(bytes);s.split("<si").skip(1).map(|x|x.split("</si>").next().unwrap_or("").split("<t").skip(1).filter_map(|t|t.split_once('>').map(|x|tag(x.1,"t").unwrap_or_else(||x.1.split("</t>").next().unwrap_or("").to_string()))).map(|t|unescape(&t)).collect::<String>()).collect()}
fn attr(s:&str,name:&str)->Option<String>{let needle=format!("{name}=\"");s.find(&needle).and_then(|i|s[i+needle.len()..].split_once('"').map(|x|x.0.to_string()))}
fn tag(s:&str,name:&str)->Option<String>{let start=format!("<{name}>");s.find(&start).and_then(|i|s[i+start.len()..].find(&format!("</{name}>")).map(|j|s[i+start.len()..i+start.len()+j].to_string()))}
fn unescape(s:&str)->String{s.replace("&amp;","&").replace("&lt;","<").replace("&gt;",">").replace("&quot;","\"").replace("&apos;","'")}
fn zip_files(bytes:&[u8])->std::result::Result<HashMap<String,Vec<u8>>,String>{let end=bytes.windows(4).rposition(|b|b==b"PK\x05\x06").ok_or("not a ZIP/XLSX file")?;let count=u16::from_le_bytes(bytes.get(end+10..end+12).ok_or("truncated ZIP")?.try_into().unwrap())as usize;let mut p=u32::from_le_bytes(bytes.get(end+16..end+20).ok_or("truncated ZIP")?.try_into().unwrap())as usize;let mut out=HashMap::new();for _ in 0..count{if bytes.get(p..p+4)!=Some(b"PK\x01\x02"){return Err("invalid ZIP index".into())}let method=u16::from_le_bytes(bytes[p+10..p+12].try_into().unwrap());let size=u32::from_le_bytes(bytes[p+20..p+24].try_into().unwrap())as usize;let name_len=u16::from_le_bytes(bytes[p+28..p+30].try_into().unwrap())as usize;let extra=u16::from_le_bytes(bytes[p+30..p+32].try_into().unwrap())as usize;let comment=u16::from_le_bytes(bytes[p+32..p+34].try_into().unwrap())as usize;let local=u32::from_le_bytes(bytes[p+42..p+46].try_into().unwrap())as usize;let name=std::str::from_utf8(&bytes[p+46..p+46+name_len]).map_err(|_|"invalid ZIP name")?.to_string();let n=u16::from_le_bytes(bytes[local+26..local+28].try_into().unwrap())as usize;let e=u16::from_le_bytes(bytes[local+28..local+30].try_into().unwrap())as usize;let data=&bytes[local+30+n+e..local+30+n+e+size];let value=if method==0{data.to_vec()}else if method==8{let mut d=DeflateDecoder::new(data);let mut v=Vec::new();d.read_to_end(&mut v).map_err(|_|"cannot decompress XLSX")?;v}else{return Err("unsupported XLSX compression".into())};out.insert(name,value);p+=46+name_len+extra+comment;}Ok(out)}
pub fn xlsx_export(jobs:&[Job])->Vec<u8>{let mut rows=String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>");let headers=["Company","Title","Location","URL","Source","Posted At","Source date caveat","Stage","Notes","Follow up on","Match score","Match reasons"];for(r,row)in std::iter::once(headers.iter().map(|s|s.to_string()).collect::<Vec<_>>()).chain(jobs.iter().map(|j|vec![j.company.clone(),j.title.clone(),j.location.clone(),j.url.clone().unwrap_or_default(),j.source.clone(),j.posted_at.clone().unwrap_or_default(),j.source_date_note.clone(),j.stage.clone(),j.notes.clone(),j.follow_up_on.clone().unwrap_or_default(),j.match_score.to_string(),j.match_reasons.join("; ")]))).enumerate(){rows.push_str(&format!("<row r=\"{}\">",r+1));for(c,v)in row.iter().enumerate(){let col=((b'A'+c as u8)as char).to_string();rows.push_str(&format!("<c r=\"{}{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",col,r+1,xml(v)));}rows.push_str("</row>")}rows.push_str("</sheetData></worksheet>");let content="<?xml version=\"1.0\" encoding=\"UTF-8\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/></Types>";let rels="<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>";let book="<?xml version=\"1.0\" encoding=\"UTF-8\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Jobs\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>";let bookrels="<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>";zip_write(&[("[Content_Types].xml",content.as_bytes()),("_rels/.rels",rels.as_bytes()),("xl/workbook.xml",book.as_bytes()),("xl/_rels/workbook.xml.rels",bookrels.as_bytes()),("xl/worksheets/sheet1.xml",rows.as_bytes())])}
fn xml(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn zip_write(files:&[(&str,&[u8])])->Vec<u8>{let mut out=Vec::new();let mut central=Vec::new();for(name,data)in files{let offset=out.len()as u32;let mut h=Hasher::new();h.update(data);let crc=h.finalize();out.extend_from_slice(b"PK\x03\x04");out.extend_from_slice(&20u16.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&crc.to_le_bytes());out.extend_from_slice(&(data.len()as u32).to_le_bytes());out.extend_from_slice(&(data.len()as u32).to_le_bytes());out.extend_from_slice(&(name.len()as u16).to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(name.as_bytes());out.extend_from_slice(data);central.extend_from_slice(b"PK\x01\x02");central.extend_from_slice(&20u16.to_le_bytes());central.extend_from_slice(&20u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&crc.to_le_bytes());central.extend_from_slice(&(data.len()as u32).to_le_bytes());central.extend_from_slice(&(data.len()as u32).to_le_bytes());central.extend_from_slice(&(name.len()as u16).to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u16.to_le_bytes());central.extend_from_slice(&0u32.to_le_bytes());central.extend_from_slice(&offset.to_le_bytes());central.extend_from_slice(name.as_bytes());}let start=out.len()as u32;out.extend_from_slice(&central);out.extend_from_slice(b"PK\x05\x06");out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out.extend_from_slice(&(files.len()as u16).to_le_bytes());out.extend_from_slice(&(files.len()as u16).to_le_bytes());out.extend_from_slice(&(central.len()as u32).to_le_bytes());out.extend_from_slice(&start.to_le_bytes());out.extend_from_slice(&0u16.to_le_bytes());out}

#[cfg(test)] mod tests { use super::*; use tempfile::tempdir; #[test] fn imports_dedupes_and_exports_xlsx(){let d=tempdir().unwrap();let s=Store::open(&d.path().join("j.db"),None).unwrap();s.migrate().unwrap();let rows=parse_import(b"Company,Title,Location,URL,Posted At\nAcme,React Engineer,Bengaluru,https://example.test/jobs/1,2026-11-01\nAcme,React Engineer,Bengaluru,https://example.test/jobs/1,2026-11-01\n","jobs.csv").unwrap();let r=s.import_jobs(rows).unwrap();assert_eq!((r.inserted,r.duplicates),(1,1));let jobs=s.list_jobs("react",Some("saved")).unwrap();assert_eq!(jobs[0].company,"Acme");let xlsx=xlsx_export(&jobs);assert_eq!(parse_import(&xlsx,"jobs.xlsx").unwrap()[0].title,"React Engineer");} }

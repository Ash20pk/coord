import"./modulepreload-polyfill-P2Xu9kJm.js";import{i as e,r as t,t as n}from"./relay-c9r9f4_p.js";var r;(function(e){e.Any=`any`,e.ApNortheast1=`ap-northeast-1`,e.ApNortheast2=`ap-northeast-2`,e.ApSouth1=`ap-south-1`,e.ApSoutheast1=`ap-southeast-1`,e.ApSoutheast2=`ap-southeast-2`,e.CaCentral1=`ca-central-1`,e.EuCentral1=`eu-central-1`,e.EuWest1=`eu-west-1`,e.EuWest2=`eu-west-2`,e.EuWest3=`eu-west-3`,e.SaEast1=`sa-east-1`,e.UsEast1=`us-east-1`,e.UsWest1=`us-west-1`,e.UsWest2=`us-west-2`})(r||={});var i;(function(e){e.abstime=`abstime`,e.bool=`bool`,e.date=`date`,e.daterange=`daterange`,e.float4=`float4`,e.float8=`float8`,e.int2=`int2`,e.int4=`int4`,e.int4range=`int4range`,e.int8=`int8`,e.int8range=`int8range`,e.json=`json`,e.jsonb=`jsonb`,e.money=`money`,e.numeric=`numeric`,e.oid=`oid`,e.reltime=`reltime`,e.text=`text`,e.time=`time`,e.timestamp=`timestamp`,e.timestamptz=`timestamptz`,e.timetz=`timetz`,e.tsrange=`tsrange`,e.tstzrange=`tstzrange`})(i||={});var a=(e,t)=>{if(e.charAt(0)===`_`)return te(t,e.slice(1,e.length));switch(e){case i.bool:return s(t);case i.float4:case i.float8:case i.int2:case i.int4:case i.int8:case i.numeric:case i.oid:return c(t);case i.json:case i.jsonb:return ee(t);case i.timestamp:return ne(t);case i.abstime:case i.date:case i.daterange:case i.int4range:case i.int8range:case i.money:case i.reltime:case i.text:case i.time:case i.timestamptz:case i.timetz:case i.tsrange:case i.tstzrange:return o(t);default:return o(t)}},o=e=>e,s=e=>{switch(e){case`t`:return!0;case`f`:return!1;default:return e}},c=e=>{if(typeof e==`string`){let t=parseFloat(e);if(!Number.isNaN(t))return t}return e},ee=e=>{if(typeof e==`string`)try{return JSON.parse(e)}catch{return e}return e},te=(e,t)=>{if(typeof e!=`string`)return e;let n=e.length-1,r=e[n];if(e[0]===`{`&&r===`}`){let r,i=e.slice(1,n);try{r=JSON.parse(`[`+i+`]`)}catch{r=i?i.split(`,`):[]}return r.map(e=>a(t,e))}return e},ne=e=>typeof e==`string`?e.replace(` `,`T`):e,re;(function(e){e.SYNC=`sync`,e.JOIN=`join`,e.LEAVE=`leave`})(re||={});var ie;(function(e){e.ALL=`*`,e.INSERT=`INSERT`,e.UPDATE=`UPDATE`,e.DELETE=`DELETE`})(ie||={});var l;(function(e){e.BROADCAST=`broadcast`,e.PRESENCE=`presence`,e.POSTGRES_CHANGES=`postgres_changes`,e.SYSTEM=`system`})(l||={});var u;(function(e){e.SUBSCRIBED=`SUBSCRIBED`,e.TIMED_OUT=`TIMED_OUT`,e.CLOSED=`CLOSED`,e.CHANNEL_ERROR=`CHANNEL_ERROR`})(u||={});function d(e){"@babel/helpers - typeof";return d=typeof Symbol==`function`&&typeof Symbol.iterator==`symbol`?function(e){return typeof e}:function(e){return e&&typeof Symbol==`function`&&e.constructor===Symbol&&e!==Symbol.prototype?`symbol`:typeof e},d(e)}function ae(e,t){if(d(e)!=`object`||!e)return e;var n=e[Symbol.toPrimitive];if(n!==void 0){var r=n.call(e,t||`default`);if(d(r)!=`object`)return r;throw TypeError(`@@toPrimitive must return a primitive value.`)}return(t===`string`?String:Number)(e)}function oe(e){var t=ae(e,`string`);return d(t)==`symbol`?t:t+``}function se(e,t,n){return(t=oe(t))in e?Object.defineProperty(e,t,{value:n,enumerable:!0,configurable:!0,writable:!0}):e[t]=n,e}function f(e,t){var n=Object.keys(e);if(Object.getOwnPropertySymbols){var r=Object.getOwnPropertySymbols(e);t&&(r=r.filter(function(t){return Object.getOwnPropertyDescriptor(e,t).enumerable})),n.push.apply(n,r)}return n}function p(e){for(var t=1;t<arguments.length;t++){var n=arguments[t]==null?{}:arguments[t];t%2?f(Object(n),!0).forEach(function(t){se(e,t,n[t])}):Object.getOwnPropertyDescriptors?Object.defineProperties(e,Object.getOwnPropertyDescriptors(n)):f(Object(n)).forEach(function(t){Object.defineProperty(e,t,Object.getOwnPropertyDescriptor(n,t))})}return e}var m=class extends Error{constructor(e,t=`storage`,n,r){super(e),this.__isStorageError=!0,this.namespace=t,this.name=t===`vectors`?`StorageVectorsError`:`StorageError`,this.status=n,this.statusCode=r}toJSON(){return{name:this.name,message:this.message,status:this.status,statusCode:this.statusCode}}},h=class extends m{constructor(e,t,n,r=`storage`,i){super(e,r,t,n),this.name=r===`vectors`?`StorageVectorsApiError`:`StorageApiError`,this.status=t,this.statusCode=n,this.code=i}toJSON(){return p(p({},super.toJSON()),{},{code:this.code})}},ce=class extends m{constructor(e,t,n=`storage`){super(e,n),this.name=n===`vectors`?`StorageVectorsUnknownError`:`StorageUnknownError`,this.originalError=t}};function le(e,t,n){let r=p({},e),i=t.toLowerCase();for(let e of Object.keys(r))e.toLowerCase()===i&&delete r[e];return r[i]=n,r}var ue=e=>{if(typeof e!=`object`||!e)return!1;let t=Object.getPrototypeOf(e);return(t===null||t===Object.prototype||Object.getPrototypeOf(t)===null)&&!(Symbol.toStringTag in e)&&!(Symbol.iterator in e)},g=e=>{if(typeof e==`object`&&e){let t=e;if(typeof t.msg==`string`)return t.msg;if(typeof t.message==`string`)return t.message;if(typeof t.error_description==`string`)return t.error_description;if(typeof t.error==`string`)return t.error;if(typeof t.error==`object`&&t.error!==null){let e=t.error;if(typeof e.message==`string`)return e.message}}return JSON.stringify(e)},de=async(e,t,n,r)=>{if(typeof e==`object`&&e&&`json`in e&&typeof e.json==`function`){let n=e,i=parseInt(String(n.status),10);Number.isFinite(i)||(i=500),n.json().then(e=>{let n=e?.statusCode||e?.code||i+``;t(new h(g(e),i,n,r,e?.code))}).catch(()=>{let e=i+``;t(new h(n.statusText||`HTTP ${i} error`,i,e,r))})}else t(new ce(g(e),e,r))},fe=(e,t,n,r)=>{let i={method:e,headers:t?.headers||{}};if(e===`GET`||e===`HEAD`||!r)return p(p({},i),n);if(ue(r)){let e=t?.headers||{},n;for(let[t,r]of Object.entries(e))t.toLowerCase()===`content-type`&&(n=r);i.headers=le(e,`Content-Type`,n??`application/json`),i.body=JSON.stringify(r)}else i.body=r;return t?.duplex&&(i.duplex=t.duplex),p(p({},i),n)};async function _(e,t,n,r,i,a,o){return new Promise((s,c)=>{e(n,fe(t,r,i,a)).then(e=>{if(!e.ok)throw e;if(r?.noResolveJson)return e;if(o===`vectors`){let t=e.headers.get(`content-type`);if(e.headers.get(`content-length`)===`0`||e.status===204||!t||!t.includes(`application/json`))return{}}return e.json()}).then(e=>s(e)).catch(e=>de(e,c,r,o))})}function pe(e=`storage`){return{get:async(t,n,r,i)=>_(t,`GET`,n,r,i,void 0,e),post:async(t,n,r,i,a)=>_(t,`POST`,n,i,a,r,e),put:async(t,n,r,i,a)=>_(t,`PUT`,n,i,a,r,e),head:async(t,n,r,i)=>_(t,`HEAD`,n,p(p({},r),{},{noResolveJson:!0}),i,void 0,e),remove:async(t,n,r,i,a)=>_(t,`DELETE`,n,i,a,r,e)}}var{get:me,post:he,put:ge,head:_e,remove:ve}=pe(`storage`),ye=`2.114.0`,v=3e4;3*v,2*v,`${ye}`;var y=`ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_`.split(``),b=` 	
\r=`.split(``);(()=>{let e=Array(128);for(let t=0;t<e.length;t+=1)e[t]=-1;for(let t=0;t<b.length;t+=1)e[b[t].charCodeAt(0)]=-2;for(let t=0;t<y.length;t+=1)e[y[t].charCodeAt(0)]=t;return e})();var be=()=>typeof window<`u`&&typeof document<`u`,x={tested:!1,writable:!1};globalThis&&(()=>{if(!be())return!1;try{if(typeof globalThis.localStorage!=`object`)return!1}catch{return!1}if(x.tested)return x.writable;let e=`lswt-${Math.random()}${Math.random()}`;try{globalThis.localStorage.setItem(e,e),globalThis.localStorage.removeItem(e),x.tested=!0,x.writable=!0}catch{x.tested=!0,x.writable=!1}return x.writable})()&&globalThis.localStorage&&globalThis.localStorage.getItem(`supabase.gotrue-js.locks.debug`);function xe(){if(typeof globalThis!=`object`)try{Object.defineProperty(Object.prototype,"__magic__",{get:function(){return this},configurable:!0}),__magic__.globalThis=__magic__,delete Object.prototype.__magic__}catch{typeof self<`u`&&(self.globalThis=self)}}new class{createNewAbortSignal(){if(this.controller){let e=Error(`Cancelling existing WebAuthn API call for new one`);e.name=`AbortError`,this.controller.abort(e)}let e=new AbortController;return this.controller=e,e.signal}cancelCeremony(){if(this.controller){let e=Error(`Manually cancelling existing WebAuthn API call`);e.name=`AbortError`,this.controller.abort(e),this.controller=void 0}}},xe();var Se=`2.114.0`,S=``,C;if(typeof Deno<`u`)S=`deno`,C=Deno.version?.deno;else if(typeof document<`u`)S=`web`;else if(typeof navigator<`u`&&navigator.product===`ReactNative`)S=`react-native`;else{var w;S=`node`;let e=globalThis.process;C=e==null||(w=e.version)==null?void 0:w.replace(/^v/,``)}var T=[`runtime=${S}`];C&&T.push(`runtime-version=${C}`),`${Se}${T.join(`; `)}`;function E(){if(typeof window<`u`||globalThis.Deno!==void 0)return!1;let e=globalThis.process;if(!e)return!1;let t=e.version;if(t==null)return!1;let n=t.match(/^v(\d+)\./);return n?parseInt(n[1],10)<=20:!1}E()&&console.warn(`⚠️  Node.js 20 and below are deprecated and will no longer be supported in future versions of @supabase/supabase-js. Please upgrade to Node.js 22 or later. For more information, visit: https://github.com/orgs/supabase/discussions/45715`);var D=null;function O(){throw Error(`Sign-in is not configured on this deployment.`)}async function k(e){let{data:t,error:n}=await O().rpc(`create_team`,{team_name:e});if(n)throw Error(n.message);return t}async function A(){throw Error(`Sign-in is not configured on this deployment.`)}async function j(e,t={}){let n={Authorization:`Bearer ${await A()}`};t.body&&(n[`Content-Type`]=`application/json`);let r=await fetch(e,{...t,headers:{...n,...t.headers}}),i=await r.json().catch(()=>({}));if(!r.ok)throw Error(i.error||`${r.status} ${r.statusText}`);return i}async function M(){let e=await A();return new WebSocket(`${n}?token=${encodeURIComponent(e)}`)}var Ce=class{onEvent;onState;onConn;claims=[];sessions=new Map;events=[];ws=null;gen=0;repo=null;stopped=!1;constructor(e,t,n){this.onEvent=e,this.onState=t,this.onConn=n}async open(e){let t=++this.gen;this.repo=e,this.close(!1),this.claims=[],this.sessions.clear(),this.events=[];try{let n=await j(`/api/events?repo=${encodeURIComponent(e)}&limit=300`);if(t!==this.gen)return;this.events=n}catch{}this.onState();let n;try{n=await M()}catch{this.onConn(!1);return}if(t!==this.gen){n.close();return}this.ws=n,n.onopen=()=>{if(t!==this.gen){n.close();return}this.onConn(!0),n.send(JSON.stringify({type:`hello`,repo:e,daemon:`console`}))},n.onmessage=e=>{if(t!==this.gen)return;let n;try{n=JSON.parse(e.data)}catch{return}n.type===`welcome`?(this.claims=n.claims??[],this.sessions=new Map((n.sessions??[]).map(e=>[e.session,e])),this.onState()):n.type===`event`&&n.event&&(this.apply(n.event),this.events.push(n.event),this.events.length>500&&this.events.shift(),this.onEvent(n.event),this.onState())},n.onerror=()=>{t===this.gen&&this.onConn(!1)},n.onclose=()=>{t!==this.gen||this.stopped||(this.onConn(!1),setTimeout(()=>{t===this.gen&&this.repo&&!this.stopped&&this.open(this.repo)},3e3))}}close(e=!0){if(e&&(this.stopped=!0,this.gen++),this.ws){try{this.ws.close()}catch{}this.ws=null}}apply(e){switch(e.type){case`claim_acquired`:this.claims=this.claims.filter(t=>t.path!==e.path),this.claims.push({session:e.session,user:e.user,path:e.path,intent:e.intent,lease_until:e.lease_until});break;case`claim_released`:case`path_freed`:this.claims=this.claims.filter(t=>t.path!==e.path);break;case`session_started`:this.sessions.set(e.session,{session:e.session,user:e.user,branch:e.branch,intent:``});break;case`intent_declared`:{let t=this.sessions.get(e.session);t&&(t.intent=e.text??``);break}case`session_ended`:this.sessions.delete(e.session),this.claims=this.claims.filter(t=>t.session!==e.session)}}},N={claim_acquired:`held`,claim_denied:`blocked`,claim_released:`plain`,path_freed:`wire`,message:`wire`,ungated_write:`warn`,cross_branch_overlap:`warn`};function we(e){switch(e.type){case`claim_denied`:return`${e.path}, held by ${e.holder_user??`unknown`}`;case`ungated_write`:return`${e.path}, over ${e.holder_user??`a peer`}’s claim`;case`intent_declared`:return`“${e.text??``}”`;case`message`:return`to ${e.to??`all`}: ${e.text??``}`;case`session_started`:return`joined on ${e.branch??`unknown branch`}`;case`session_ended`:return`left`;default:return e.path??``}}var P=e=>{if(!e)return`never`;let t=Date.now()-e;return t<6e4?`just now`:t<36e5?`${Math.floor(t/6e4)} min ago`:t<864e5?`${Math.floor(t/36e5)} h ago`:`${Math.floor(t/864e5)} d ago`},F=(e,t=document)=>t.querySelector(e),Te=F(`#auth`),I=F(`#shell`),Ee=F(`#booting`),L=F(`#view`),R=null,De=`member`,z=null,B=null,V=null,H=location.hash===`#signup`?`signup`:`signin`;function U(){let e=H===`signup`;F(`#auth-title`).textContent=e?`Create your account`:`Sign in`,F(`#auth-sub`).textContent=e?`A team, an agent token, and a live log of every session. No card needed.`:`Manage your team, agent tokens and live sessions.`,F(`#auth-go`).textContent=e?`Create account`:`Sign in`,F(`#auth-switch`).textContent=e?`I already have an account`:`Create an account`,F(`#team-field`).hidden=!e,F(`#auth-team`).required=e,F(`#auth-password`).autocomplete=e?`new-password`:`current-password`}function W(e,t=``){let n=F(`#auth-err`),r=F(`#auth-ok`);n.hidden=e!==`err`,r.hidden=e!==`ok`,e===`err`&&(n.textContent=t),e===`ok`&&(r.textContent=t)}function G(){Ee.hidden=!0,I.hidden=!0,Te.hidden=!1,U(),W(`err`,`Sign-in is not configured on this deployment. Set VITE_SUPABASE_URL and VITE_SUPABASE_ANON_KEY, or run your own relay and use an agent token.`),F(`#auth-go`).disabled=!0}F(`#auth-switch`).addEventListener(`click`,()=>{H=H===`signup`?`signin`:`signup`,location.hash=H===`signup`?`#signup`:``,W(`clear`),U()}),F(`#auth-reset`).addEventListener(`click`,async()=>{let e=F(`#auth-email`).value.trim();if(!e){W(`err`,`Enter your email address first, then choose Forgot password.`);return}try{let{error:t}=await D.auth.resetPasswordForEmail(e,{redirectTo:`${location.origin}/app/`});if(t)throw Error(t.message);W(`ok`,`Check ${e} for a link to set a new password.`)}catch(e){W(`err`,e.message)}}),F(`#auth-form`).addEventListener(`submit`,async e=>{e.preventDefault();let t=F(`#auth-go`),n=F(`#auth-email`).value.trim(),r=F(`#auth-password`).value,i=F(`#auth-team`).value.trim();W(`clear`),t.disabled=!0,t.textContent=H===`signup`?`Creating account`:`Signing in`;try{let e=D;if(H===`signup`){let{data:t,error:a}=await e.auth.signUp({email:n,password:r});if(a)throw Error(a.message);if(!t.session){W(`ok`,`Check ${n} to confirm your address, then sign in.`),H=`signin`,U();return}await k(i||`${n.split(`@`)[0]}'s team`)}else{let{error:t}=await e.auth.signInWithPassword({email:n,password:r});if(t)throw Error(t.message)}await $()}catch(e){W(`err`,e.message)}finally{t.disabled=!1,U()}}),F(`#signout`).addEventListener(`click`,async()=>{B?.close(),await D?.auth.signOut(),R=null,location.hash=``,G()});var Oe=[`sessions`,`repositories`,`tokens`,`team`,`settings`];function K(){let e=location.hash.replace(`#`,``);return Oe.includes(e)?e:`sessions`}function ke(){let e=K();for(let t of document.querySelectorAll(`.tabs a`))t.classList.toggle(`on`,t.getAttribute(`href`)===`#${e}`)}function q(){switch(ke(),B?.close(),B=null,K()){case`sessions`:return Ae();case`repositories`:return Pe();case`tokens`:return Z();case`team`:return Fe();case`settings`:return Ie()}}function Ae(){let n=z?.repos??[];if(L.innerHTML=`
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Sessions</h1>
          <p>Every agent currently working a repository your team has connected, and the event log behind them.</p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head">
          <h2>Live</h2>
          ${n.length?`<select class="picker" id="repo-pick" aria-label="Repository">${n.map(e=>`<option value="${t(e.repo)}">${t(e.repo)}</option>`).join(``)}</select>`:``}
          <span class="state" id="conn">idle</span>
          <div class="right"><div class="counts" id="counts"></div></div>
        </div>
        <div id="presence"></div>
        <div class="log" id="log">
          <div class="row h"><span>time</span><span>agent</span><span>event</span><span>detail</span></div>
          <div id="log-rows"></div>
        </div>
      </div>
    </div>`,!n.length){F(`#log-rows`).innerHTML=J(),e(L);return}let r=F(`#repo-pick`);V&&n.some(e=>e.repo===V)&&(r.value=V),V=r.value,r.addEventListener(`change`,()=>{V=r.value,Y()}),Y()}function J(){return`<div class="empty">
    No repository has connected yet. Enrol one where your agents run, then start the daemon.
    <div class="cmd-row"><code>knoot init --relay ${t(n)}</code><button class="copy" type="button">Copy</button></div>
    <div class="cmd-row"><code>knoot daemon</code><button class="copy" type="button">Copy</button></div>
  </div>`}function Y(){let e=F(`#log-rows`),t=F(`#log`);B?.close(),B=new Ce(n=>Me(n,e,t),()=>{Ne(),e.dataset.seeded||je(e,t)},e=>{let t=F(`#conn`);t.textContent=e===!0?`live`:e===!1?`reconnecting`:`idle`,t.className=`state`+(e===!0?` live`:e===!1?` off`:``)}),B.open(V)}function X(e,n){let r=N[e.type]??`plain`,i=e.ts?new Date(e.ts).toLocaleTimeString([],{hour12:!1}).slice(0,8):``;return`<div class="row${r===`blocked`?` is-blocked`:``}${n?` enter`:``}">
    <span class="t">${t(i)}</span><span class="u">${t(e.user??``)}</span>
    <span class="k ${r}">${t(e.type)}</span><span class="d">${t(we(e))}</span></div>`}function je(e,t){let n=B?.events??[];e.innerHTML=n.length?n.slice(-300).map(e=>X(e,!1)).join(``):`<div class="empty">Connected. Nothing has happened in this repository yet.</div>`,e.dataset.seeded=`1`,t.scrollTop=t.scrollHeight}function Me(e,t,n){let r=n.scrollTop+n.clientHeight>=n.scrollHeight-30;for(t.querySelector(`.empty`)&&(t.innerHTML=``),t.insertAdjacentHTML(`beforeend`,X(e,!0));t.children.length>300;)t.removeChild(t.firstChild);r&&(n.scrollTop=n.scrollHeight)}function Ne(){let e=F(`#presence`);if(!e||!B)return;let n=[...B.sessions.values()],r=new Set(B.events.slice(-60).filter(e=>e.type===`claim_denied`).map(e=>e.session));e.innerHTML=n.length?`<table class="rows">
        <thead><tr><th>Agent</th><th>Working on</th><th>Holds</th></tr></thead>
        <tbody>${n.map(e=>{let n=B.claims.filter(t=>t.session===e.session).map(e=>e.path),i=r.has(e.session)&&!n.length,a=n.length?`holds`:i?`holds blocked`:`holds none`,o=n.length?n.join(`  `):i?`blocked, waiting`:`nothing`;return`<tr><td class="mono">${t(e.user??e.session.slice(0,8))}</td>
            <td class="dim">${t(e.intent||`no stated intent yet`)}</td>
            <td class="${a}">${t(o)}</td></tr>`}).join(``)}</tbody></table>`:``;let i=n.length,a=B.claims.length,o=r.size,s=F(`#counts`);s&&(s.innerHTML=`<span><b>${i}</b>session${i===1?``:`s`}</span><span><b>${a}</b>claim${a===1?``:`s`}</span>`+(o?`<span class="blocked"><b>${o}</b>blocked</span>`:``))}function Pe(){let n=z?.repos??[];L.innerHTML=`
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Repositories</h1>
          <p>A repository appears here the first time an agent on it reaches the relay. Nothing to create by hand.</p>
        </div>
      </div>
      <div class="panel">
        ${n.length?`<table class="rows">
          <thead><tr><th>Repository</th><th>Last activity</th><th></th></tr></thead>
          <tbody>${n.map(e=>`<tr>
            <td class="mono">${t(e.repo)}</td>
            <td class="dim">${t(P(e.last_seen_ts??null))}</td>
            <td class="right"><a class="btn quiet sm" href="#sessions" data-repo="${t(e.repo)}">Open log</a></td>
          </tr>`).join(``)}</tbody></table>`:J()}
      </div>
    </div>`;for(let e of L.querySelectorAll(`[data-repo]`))e.addEventListener(`click`,()=>{V=e.dataset.repo});e(L)}function Z(){let r=z?.tokens??[];L.innerHTML=`
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Agent tokens</h1>
          <p>Machines authenticate with tokens, not with your password. Give each machine its own so revoking one costs you nothing else. Tokens are stored as hashes and can never be shown again.</p>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>Tokens</h2><div class="right"><span class="state">${r.filter(e=>!e.revoked).length} live</span></div></div>
        <div class="panel-body">
          <div class="inline-form">
            <input id="mint-label" maxlength="40" placeholder="Label, such as laptop or ci">
            <button class="btn" id="mint-go">Mint token</button>
          </div>
          <div id="mint-out"></div>
          <div class="err" id="tok-err" hidden></div>
        </div>
        ${r.length?`<table class="rows">
          <thead><tr><th>Label</th><th>Created</th><th>Last used</th><th></th></tr></thead>
          <tbody>${r.map(e=>`<tr>
            <td><span class="${e.revoked?`strike`:``}">${t(e.label||`unlabelled`)}</span>${e.id===z?.token_id?`<span class="tag mine">this console</span>`:``}${e.revoked?`<span class="tag dead">revoked</span>`:``}</td>
            <td class="dim">${t(P(e.created_ts))}</td>
            <td class="dim">${e.revoked?``:t(P(e.last_seen_ts))}</td>
            <td class="right">${e.revoked?``:`<button class="btn danger sm" data-revoke="${t(e.id)}">Revoke</button>`}</td>
          </tr>`).join(``)}</tbody></table>`:``}
      </div>

      <div class="panel">
        <div class="panel-head"><h2>Use a token</h2></div>
        <div class="panel-body steps">
          <div class="step"><p>Install the binary on the machine that runs agents.</p>
            <div class="cmd-row"><code>cargo install --git https://github.com/Ash20pk/knoot</code><button class="copy" type="button">Copy</button></div></div>
          <div class="step"><p>Enrol the repository once, then commit what it writes.</p>
            <div class="cmd-row"><code>knoot init --relay ${t(n)}</code><button class="copy" type="button">Copy</button></div></div>
          <div class="step"><p>Store the token on that machine and run the daemon.</p>
            <div class="cmd-row"><code>knoot login --relay ${t(n)} --token &lt;token&gt;</code><button class="copy" type="button">Copy</button></div>
            <div class="cmd-row"><code>knoot daemon</code><button class="copy" type="button">Copy</button></div></div>
        </div>
      </div>
    </div>`,e(L),F(`#mint-go`).addEventListener(`click`,async()=>{let e=F(`#mint-go`),n=F(`#tok-err`);n.hidden=!0,e.disabled=!0;try{let e=F(`#mint-label`).value.trim(),n=await j(`/api/tokens`,{method:`POST`,body:JSON.stringify({label:e})});F(`#mint-out`).innerHTML=`<div class="reveal">
        <div class="lbl">New token. This is the only time it is readable.</div>
        <div class="val">${t(n.token)}</div></div>`,await Q()}catch(e){n.textContent=e.message,n.hidden=!1}finally{e.disabled=!1}});for(let e of L.querySelectorAll(`[data-revoke]`))e.addEventListener(`click`,async()=>{if(confirm(`Revoke this token? Machines using it stop coordinating. They fail open, so their agents keep working alone.`)){e.disabled=!0;try{await j(`/api/tokens/${encodeURIComponent(e.dataset.revoke)}/revoke`,{method:`POST`}),await Q(),Z()}catch(t){let n=F(`#tok-err`);n.textContent=t.message,n.hidden=!1,e.disabled=!1}}})}async function Fe(){L.innerHTML=`
    <div class="page">
      <div class="page-head">
        <div>
          <h1>Team</h1>
          <p>Everyone here can see the log and manage agent tokens. You are signed in as ${t(De)}.</p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Members</h2></div>
        <div id="members"><div class="empty">Loading members.</div></div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Invite a teammate</h2></div>
        <div class="panel-body">
          <p>Send them the sign-up link and the team name. They join this team when they create an account with an email on your domain.</p>
          <div class="cmd-row" style="margin-top:14px"><code>${t(location.origin)}/app/#signup</code><button class="copy" type="button">Copy</button></div>
        </div>
      </div>
    </div>`,e(L);try{let{data:e,error:n}=await D.from(`team_members`).select(`user_id, email, role, created_at`);if(n)throw Error(n.message);let r=e??[];F(`#members`).innerHTML=r.length?`<table class="rows"><thead><tr><th>Email</th><th>Role</th><th>Joined</th></tr></thead>
         <tbody>${r.map(e=>`<tr><td>${t(e.email)}</td><td class="dim">${t(e.role)}</td>
           <td class="dim">${t(P(Date.parse(e.created_at)))}</td></tr>`).join(``)}</tbody></table>`:`<div class="empty">Just you so far.</div>`}catch(e){F(`#members`).innerHTML=`<div class="empty">Could not load members: ${t(e.message)}</div>`}}function Ie(){L.innerHTML=`
    <div class="page">
      <div class="page-head"><div><h1>Settings</h1><p>Account and relay details.</p></div></div>
      <div class="panel">
        <div class="panel-head"><h2>Relay</h2></div>
        <div class="panel-body">
          <p>Your agents connect to this address. It is the same host that served this page.</p>
          <div class="cmd-row" style="margin-top:14px"><code>${t(n)}</code><button class="copy" type="button">Copy</button></div>
          <p style="margin-top:16px">Team id <code>${t(z?.team_id??``)}</code></p>
        </div>
      </div>
      <div class="panel">
        <div class="panel-head"><h2>Password</h2></div>
        <div class="panel-body">
          <label class="field" style="max-width:400px;margin-top:0">
            <span>New password</span>
            <input id="new-password" type="password" minlength="8" autocomplete="new-password" placeholder="At least 8 characters">
          </label>
          <button class="btn" id="pw-go" style="margin-top:14px">Change password</button>
          <div class="err" id="pw-err" hidden></div>
          <div class="ok" id="pw-ok" hidden></div>
        </div>
      </div>
    </div>`,e(L),F(`#pw-go`).addEventListener(`click`,async()=>{let e=F(`#pw-err`),t=F(`#pw-ok`);e.hidden=!0,t.hidden=!0;let n=F(`#new-password`).value;if(n.length<8){e.textContent=`Use at least 8 characters.`,e.hidden=!1;return}let{error:r}=await D.auth.updateUser({password:n});if(r){e.textContent=r.message,e.hidden=!1;return}t.textContent=`Password changed.`,t.hidden=!1,F(`#new-password`).value=``})}async function Q(){z=await j(`/api/team`)}async function $(){G()}addEventListener(`hashchange`,()=>{if(I.hidden){H=location.hash===`#signup`?`signup`:`signin`,U();return}q()}),setInterval(async()=>{if(!I.hidden&&R)try{let e=(z?.repos??[]).map(e=>e.repo).join();await Q();let t=(z?.repos??[]).map(e=>e.repo).join();(e!==t&&K()!==`sessions`||e!==t&&!V)&&q()}catch{}},2e4),$();
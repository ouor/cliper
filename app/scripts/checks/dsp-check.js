// Renders the bundled preview worklet in an OfflineAudioContext with synthetic stems and
// compares each listening mode against the docs/ipc.md formula. Run via scripts/check.mjs.
(async()=>{
 const url = window.__cliper.workletUrl;
 const SR=48000, N=SR*2;
 const results={url};
 // synthetic stems: orig = sin 220Hz * 0.5 + 0.2, vocals = sin 440Hz * 0.3
 const o=new Int16Array(N*2), v=new Int16Array(N*2);
 const fo=i=>0.5*Math.sin(2*Math.PI*220*i/SR)+0.2, fv=i=>0.3*Math.sin(2*Math.PI*440*i/SR);
 for(let i=0;i<N;i++){o[2*i]=o[2*i+1]=Math.round(fo(i)*32767);v[2*i]=v[2*i+1]=Math.round(fv(i)*32767);}
 const q=x=>Math.round(x*32767)/32768; // int16 quantisation used by the loader/worklet
 const origin=100; // VOD seconds at frame 0
 // long mute, overlapping −12 dB mute, short mute (fades clamped to len/2 each, as render.rs),
 // and a gain_db ≥ 0 mute (floor 1 = no effect)
 const mutes=[{s:100.5,e:101.2,fi:0.08,fo:0.3,floor:0},{s:101.0,e:101.6,fi:0.05,fo:0.05,floor:Math.pow(10,-12/20)},{s:101.75,e:101.95,fi:0.08,fo:0.3,floor:0},{s:100.1,e:100.4,fi:0.05,fo:0.05,floor:1}];
 const gain=(t)=>{let g=1;for(const m of mutes){if(t<m.s||t>m.e)continue;const half=(m.e-m.s)/2;const fi=Math.min(m.fi,half),fo2=Math.min(m.fo,half);let w;if(fi>0&&t<m.s+fi)w=0.5*(1+Math.cos(Math.PI*(t-m.s)/fi));else if(fo2>0&&t>m.e-fo2)w=0.5*(1-Math.cos(Math.PI*(t-(m.e-fo2))/fo2));else w=0;const val=m.floor+(1-m.floor)*w;if(val<g)g=val}return g};
 for (const mode of [0,1,2,3]) {
  const ctx=new OfflineAudioContext(2,N-SR/10,SR);
  await ctx.audioWorklet.addModule(url);
  const node=new AudioWorkletNode(ctx,"cliper-player",{numberOfInputs:0,numberOfOutputs:1,outputChannelCount:[2]});
  node.connect(ctx.destination);
  node.port.postMessage({type:"stems:init",gen:1,origin,frames:N,orig:new Int16Array(N*2),voc:new Int16Array(N*2)});
  node.port.postMessage({type:"stems:data",gen:1,which:0,offset:0,data:o.slice()});
  node.port.postMessage({type:"stems:data",gen:1,which:1,offset:0,data:v.slice()});
  node.port.postMessage({type:"mutes",mutes});
  node.port.postMessage({type:"muteOn",on:true});
  node.port.postMessage({type:"volume",volume:1});
  node.port.postMessage({type:"mode",mode});
  node.port.postMessage({type:"play",pos:origin,rate:1});
  await new Promise(r=>setTimeout(r,50));
  const buf=await ctx.startRendering();
  const L=buf.getChannelData(0);
  let maxErr=0, at=-1;
  // skip the first 512 samples (mode cross-fade from the initial mode 0)
  for(let i=512;i<L.length;i++){const t=origin+i/SR;const g=gain(t);const O=q(fo(i)),V=q(fv(i));const exp=[O,O-(1-g)*V,(1-g)*V,O-V][mode];const e=Math.abs(L[i]-exp);if(e>maxErr){maxErr=e;at=i}}
  results["mode"+mode]={maxErr:+maxErr.toExponential(2),at:+(at/SR).toFixed(4)};
 }
 return results;
})()

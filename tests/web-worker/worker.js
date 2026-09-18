import init, {start} from './pkg/smoke.js';
Error.stackTraceLimit=100;
onmessage=async ({data:{module,memory,id,canvas}})=>{try{await init({module_or_path:module,memory}); start(id,canvas);postMessage('ready');}catch(e){console.error(e);postMessage('error:'+e.stack);}};

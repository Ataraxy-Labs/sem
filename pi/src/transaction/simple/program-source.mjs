// Keep parser identity and line-oriented source distinct. Never silently widen
// an entity edit to include another declaration on the same line.
export function programSource(bytes,entity) {
 const {start,end}=entity;
 const lineStart=start===0?0:bytes.lastIndexOf(10,start-1)+1;
 const last=end>start?end-1:start;
 const newline=bytes.indexOf(10,last);
 const lineEnd=newline<0?bytes.length:newline;
 return {
  content:bytes.subarray(start,end).toString('utf8'),
  source_range:{start_byte:start,end_byte:end},
  line_content:bytes.subarray(lineStart,lineEnd).toString('utf8'),
  line_range:{start_byte:lineStart,end_byte:lineEnd},
  source_representation:'content=exact_parser_span; line_content=full_lines_without_final_LF'
 };
}

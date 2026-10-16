"""The 27 wait --new screen-paint regression cases from the sandbox harness."""
E = "\x1b"
# name, kind, needle, pre (printed before arm), post (event-gated chunks after arm), expect rc
SPIN = []
for i in range(40):
    SPIN += ["\b" + "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"[i % 10]]
CASES = [
 ("A1_plain","A","answer","",["answer\r\n"],0),
 ("A2_styled_midword","A","answer","",[f"ans{E}[1;31mwer{E}[0m\r\n"],0),
 ("A3_split_frames","A","answer","",["an","sw","er\r\n"],0),
 ("A4_scrolled_then_reprint","A","answer","answer\r\n",["x\r\n"*40,"answer\r\n"],0),
 ("A5a_cup_out_of_order","A","answer","",[f"{E}[10;4Hwer{E}[10;1Hans"],0),
 ("A5b_cursor_forward_space","A","hello world","",[f"hello{E}[1Cworld"],0),
 ("A5c_cursor_back_overpaint","A","answer","",[f"answxx{E}[2Der"],0),
 ("A6_clear_reprint","A","answer","answer",[f"{E}[2J{E}[Hanswer"],0),
 ("A7_utf8_split","A","añswér","",[b"a\xc3".decode('latin1'),b"\xb1sw\xc3".decode('latin1'),b"\xa9r\r\n".decode('latin1')],0),
 ("B1_already_on_screen","B","answer","answer\r\n",["unrelated\r\n"],1),
 ("B2a_inplace_append","B","answer","prompt: answer",["!"],1),
 ("B2b_inplace_complete","B","answer","answe",["r"],1),
 ("B2c_inplace_prefix","B","answer","nswer",[f"{E}[1;1Ha"],1),
 ("B3_spinner_churn","B","answer","answer working  ",SPIN,1),
 ("B4_prompt_scrolled_up","B","answer",f"{E}[20;1H> answer",[f"{E}[3S{E}[20;1H> "],1),
 ("B5a_join_across_gap","B","answer","",[f"ans{E}[10Cwer"],1),
 ("B5b_join_across_rows","B","answer","",[f"ans{E}[2;1Hwer"],1),
 ("B5c_join_wrap_unrelated","B","answer","",[f"{E}[5;78Hans{E}[6;1Hwer"],1),
 ("B7_partial_overpaint","B","answer","answer",[f"{E}[1;1Han"],1),
 ("B8_altscreen_roundtrip","B","answer","answer\r\n",[f"{E}[?1049hfoo{E}[?1049l"],1),
 ("B9a_delete_char_shift","B","answer","xxanswer",[f"{E}[1;1H{E}[2P"],1),
 ("B9b_insert_char_shift","B","answer","answer",[f"{E}[1;1H{E}[3@"],1),
 ("B9c_insert_line_shift","B","answer","answer",[f"{E}[1;1H{E}[2L"],1),
 ("B10_spaced_gap_nonblank","B","hello world","hello Xworld",[f"{E}[1;1Hhello{E}[1;8Hworld"],1),
 ("A8_wide_cup","A","日本語","",[f"{E}[3;3H本語{E}[3;1H日"],0),
 ("A9_overpaint_completes_new","A","answer","",[f"axxxxr{E}[1;2Hnswe"],0),
 ("B6_only_in_scrollback","B","answer","answer\r\n"+"y\r\n"*40,["z\r\n"*3],1),
]

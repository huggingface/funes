import json, collections, sys

SP = os.environ.get("LME_WORK", ".")
PER_TYPE = 7   # non-abstention questions per question_type
N_ABS = 8      # abstention questions (qid ends _abs)

qs = json.load(open(f"{SP}/longmemeval_s_cleaned.json"))
by_type = collections.defaultdict(list)
absl = []
for q in qs:
    (absl if q["question_id"].endswith("_abs") else by_type[q["question_type"]]).append(q)

sel = []
for t in sorted(by_type):
    sel += sorted(by_type[t], key=lambda x: x["question_id"])[:PER_TYPE]
sel += sorted(absl, key=lambda x: x["question_id"])[:N_ABS]

json.dump(sel, open(f"{SP}/sample50.json", "w"))

print(f"corpus: {len(qs)} questions; selected: {len(sel)}")
print("by (question_type, is_abstention):")
for k, v in sorted(collections.Counter(
        (q["question_type"], q["question_id"].endswith("_abs")) for q in sel).items()):
    print(f"  {k}: {v}")
print(f"avg sessions/q: {sum(len(q['haystack_sessions']) for q in sel)/len(sel):.1f}")
print(f"avg turns/q:    {sum(sum(len(s) for s in q['haystack_sessions']) for q in sel)/len(sel):.0f}")

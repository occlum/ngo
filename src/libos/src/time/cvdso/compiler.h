#ifndef __UTIL_COMPILER_H__
#define __UTIL_COMPILER_H__

#define barrier() __asm__ __volatile__("": : :"memory")

#define mb() 	__asm__ __volatile__("mfence":::"memory")
#define rmb()	__asm__ __volatile__("lfence":::"memory")

#define smp_mb()	mb()
#define smp_rmb()	rmb()

#define cpu_relax()   rep_nop()
static inline void rep_nop(void)
{
	__asm__ __volatile__("rep;nop": : :"memory");
}

#define likely(x)		__builtin_expect(!!(x), 1)
#define unlikely(x)		__builtin_expect(!!(x), 0)

#define inline			__inline__ __attribute__((always_inline))

#define unused(x)		{__typeof__(x) __attribute__((unused)) d=(x);}

#define __maybe_unused		__attribute__((unused))
#define __force

#define __ACCESS_ONCE(x) ({ \
	 __maybe_unused __typeof__(x) __var = (__force __typeof__(x)) 0; \
	(volatile __typeof__(x) *)&(x); })
#define ACCESS_ONCE(x) (*__ACCESS_ONCE(x))


#define DECLARE_ARGS(val, low, high)	unsigned long low, high
#define EAX_EDX_VAL(val, low, high)	((low) | (high) << 32)
#define EAX_EDX_RET(val, low, high)	"=a" (low), "=d" (high)

#define X86_FEATURE_LFENCE_RDTSC	( 3*32+18) /* "" LFENCE synchronizes RDTSC */
#define X86_FEATURE_RDTSCP		( 1*32+27) /* RDTSCP */

#define b_replacement(num)	"664"#num
#define e_replacement(num)	"665"#num

#define alt_end_marker		"663"
#define alt_slen		"662b-661b"
#define alt_pad_len		alt_end_marker"b-662b"
#define alt_total_slen		alt_end_marker"b-661b"
#define alt_rlen(num)		e_replacement(num)"f-"b_replacement(num)"f"

#define alt_max_short(a, b)	"((" a ") ^ (((" a ") ^ (" b ")) & -(-((" a ") < (" b ")))))"

#define OLDINSTR_2(oldinstr, num1, num2) \
	"# ALT: oldinstr2\n"									\
	"661:\n\t" oldinstr "\n662:\n"								\
	"# ALT: padding2\n"									\
	".skip -((" alt_max_short(alt_rlen(num1), alt_rlen(num2)) " - (" alt_slen ")) > 0) * "	\
		"(" alt_max_short(alt_rlen(num1), alt_rlen(num2)) " - (" alt_slen ")), 0x90\n"	\
	alt_end_marker ":\n"

#define __stringify_1(x...)	#x
#define __stringify(x...)	__stringify_1(x)

#define ALTINSTR_ENTRY(feature, num)					      \
	" .long 661b - .\n"				/* label           */ \
	" .long " b_replacement(num)"f - .\n"		/* new instruction */ \
	" .word " __stringify(feature) "\n"		/* feature bit     */ \
	" .byte " alt_total_slen "\n"			/* source len      */ \
	" .byte " alt_rlen(num) "\n"			/* replacement len */ \
	" .byte " alt_pad_len "\n"			/* pad len */

#define ALTINSTR_REPLACEMENT(newinstr, feature, num)	/* replacement */	\
	"# ALT: replacement " #num "\n"						\
	b_replacement(num)":\n\t" newinstr "\n" e_replacement(num) ":\n"

#define ALTERNATIVE_2(oldinstr, newinstr1, feature1, newinstr2, feature2)\
	OLDINSTR_2(oldinstr, 1, 2)					\
	".pushsection .altinstructions,\"a\"\n"				\
	ALTINSTR_ENTRY(feature1, 1)					\
	ALTINSTR_ENTRY(feature2, 2)					\
	".popsection\n"							\
	".pushsection .altinstr_replacement, \"ax\"\n"			\
	ALTINSTR_REPLACEMENT(newinstr1, feature1, 1)			\
	ALTINSTR_REPLACEMENT(newinstr2, feature2, 2)			\
	".popsection\n"
#endif
// trans.rs: Translate the completed AST to the LLVM IR.
//
// Some functions here, such as trans_block and trans_expr, return a value --
// the result of the translation to LLVM -- while others, such as trans_fn,
// trans_impl, and trans_item, are called only for the side effect of adding a
// particular definition to the LLVM IR output we're producing.
//
// Hopefully useful general knowledge about trans:
//
//   * There's no way to find out the ty::t type of a ValueRef.  Doing so
//     would be "trying to get the eggs out of an omelette" (credit:
//     pcwalton).  You can, instead, find out its TypeRef by calling val_ty,
//     but many TypeRefs correspond to one ty::t; for instance, tup(int, int,
//     int) and rec(x=int, y=int, z=int) will have the same TypeRef.

import core::ctypes::c_uint;
import std::{map, time};
import std::map::hashmap;
import std::map::{new_int_hash, new_str_hash};
import option::{some, none};
import driver::session;
import session::session;
import front::attr;
import middle::{ty, gc, resolve, debuginfo};
import middle::freevars::*;
import back::{link, abi, upcall};
import syntax::{ast, ast_util, codemap};
import syntax::visit;
import syntax::codemap::span;
import syntax::print::pprust::{expr_to_str, stmt_to_str, path_to_str};
import pat_util::*;
import visit::vt;
import util::common::*;
import lib::llvm::{llvm, mk_target_data, mk_type_names};
import lib::llvm::llvm::{ModuleRef, ValueRef, TypeRef, BasicBlockRef};
import lib::llvm::{True, False};
import link::{mangle_internal_name_by_type_only,
              mangle_internal_name_by_seq,
              mangle_internal_name_by_path,
              mangle_internal_name_by_path_and_seq,
              mangle_exported_name};
import metadata::{csearch, cstore};
import util::ppaux::{ty_to_str, ty_to_short_str};

import trans_common::*;
import trans_build::*;
import tvec = trans_vec;

fn type_of_1(bcx: @block_ctxt, t: ty::t) -> TypeRef {
    let cx = bcx_ccx(bcx);
    check type_has_static_size(cx, t);
    type_of(cx, bcx.sp, t)
}

fn type_of(cx: @crate_ctxt, sp: span, t: ty::t) : type_has_static_size(cx, t)
   -> TypeRef {
    // Should follow from type_has_static_size -- argh.
    // FIXME (requires Issue #586)
    check non_ty_var(cx, t);
    type_of_inner(cx, sp, t)
}

fn type_of_explicit_args(cx: @crate_ctxt, sp: span, inputs: [ty::arg]) ->
   [TypeRef] {
    let atys = [];
    for arg in inputs {
        let arg_ty = arg.ty;
        // FIXME: would be nice to have a constraint on arg
        // that would obviate the need for this check
        check non_ty_var(cx, arg_ty);
        let llty = type_of_inner(cx, sp, arg_ty);
        atys += [arg.mode == ast::by_val ? llty : T_ptr(llty)];
    }
    ret atys;
}


// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_llargs_for_fn_args.
//  - new_fn_ctxt
//  - trans_args
fn type_of_fn(cx: @crate_ctxt, sp: span, inputs: [ty::arg],
              output: ty::t, params: [ty::param_bounds]) -> TypeRef {
    let atys: [TypeRef] = [];

    // Arg 0: Output pointer.
    check non_ty_var(cx, output);
    let out_ty = T_ptr(type_of_inner(cx, sp, output));
    atys += [out_ty];

    // Arg 1: Environment
    atys += [T_opaque_cbox_ptr(cx)];

    // Args >2: ty params, if not acquired via capture...
    for bounds in params {
        atys += [T_ptr(cx.tydesc_type)];
        for bound in *bounds {
            alt bound {
              ty::bound_iface(_) { atys += [T_ptr(T_dict())]; }
              _ {}
            }
        }
    }
    // ... then explicit args.
    atys += type_of_explicit_args(cx, sp, inputs);
    ret T_fn(atys, llvm::LLVMVoidType());
}

// Given a function type and a count of ty params, construct an llvm type
fn type_of_fn_from_ty(cx: @crate_ctxt, sp: span, fty: ty::t,
                      param_bounds: [ty::param_bounds]) -> TypeRef {
    // FIXME: Check should be unnecessary, b/c it's implied
    // by returns_non_ty_var(t). Make that a postcondition
    // (see Issue #586)
    let ret_ty = ty::ty_fn_ret(cx.tcx, fty);
    ret type_of_fn(cx, sp, ty::ty_fn_args(cx.tcx, fty),
                   ret_ty, param_bounds);
}

fn type_of_inner(cx: @crate_ctxt, sp: span, t: ty::t)
    : non_ty_var(cx, t) -> TypeRef {
    // Check the cache.

    if cx.lltypes.contains_key(t) { ret cx.lltypes.get(t); }
    let llty = alt ty::struct(cx.tcx, t) {
      ty::ty_native(_) { T_ptr(T_i8()) }
      ty::ty_nil. { T_nil() }
      ty::ty_bot. {
        T_nil() /* ...I guess? */
      }
      ty::ty_bool. { T_bool() }
      ty::ty_int(t) { T_int_ty(cx, t) }
      ty::ty_uint(t) { T_uint_ty(cx, t) }
      ty::ty_float(t) { T_float_ty(cx, t) }
      ty::ty_str. { T_ptr(T_vec(cx, T_i8())) }
      ty::ty_tag(did, _) { type_of_tag(cx, sp, did, t) }
      ty::ty_box(mt) {
        let mt_ty = mt.ty;
        check non_ty_var(cx, mt_ty);
        T_ptr(T_box(cx, type_of_inner(cx, sp, mt_ty))) }
      ty::ty_uniq(mt) {
        let mt_ty = mt.ty;
        check non_ty_var(cx, mt_ty);
        T_ptr(type_of_inner(cx, sp, mt_ty)) }
      ty::ty_vec(mt) {
        let mt_ty = mt.ty;
        if ty::type_has_dynamic_size(cx.tcx, mt_ty) {
            T_ptr(cx.opaque_vec_type)
        } else {
            // should be unnecessary
            check non_ty_var(cx, mt_ty);
            T_ptr(T_vec(cx, type_of_inner(cx, sp, mt_ty))) }
      }
      ty::ty_ptr(mt) {
        let mt_ty = mt.ty;
        check non_ty_var(cx, mt_ty);
        T_ptr(type_of_inner(cx, sp, mt_ty)) }
      ty::ty_rec(fields) {
        let tys: [TypeRef] = [];
        for f: ty::field in fields {
            let mt_ty = f.mt.ty;
            check non_ty_var(cx, mt_ty);
            tys += [type_of_inner(cx, sp, mt_ty)];
        }
        T_struct(tys)
      }
      ty::ty_fn(_) {
        T_fn_pair(cx, type_of_fn_from_ty(cx, sp, t, []))
      }
      ty::ty_native_fn(args, out) {
        let nft = native_fn_wrapper_type(cx, sp, [], t);
        T_fn_pair(cx, nft)
      }
      ty::ty_iface(_, _) { T_opaque_iface_ptr(cx) }
      ty::ty_res(_, sub, tps) {
        let sub1 = ty::substitute_type_params(cx.tcx, tps, sub);
        check non_ty_var(cx, sub1);
        // FIXME #1184: Resource flag is larger than necessary
        ret T_struct([cx.int_type, type_of_inner(cx, sp, sub1)]);
      }
      ty::ty_var(_) {
        // Should be unreachable b/c of precondition.
        // FIXME: would be nice to have a way of expressing this
        // through postconditions, and then making it sound to omit
        // cases in the alt
        std::util::unreachable()
      }
      ty::ty_param(_, _) { T_typaram(cx.tn) }
      ty::ty_send_type. | ty::ty_type. { T_ptr(cx.tydesc_type) }
      ty::ty_tup(elts) {
        let tys = [];
        for elt in elts {
            check non_ty_var(cx, elt);
            tys += [type_of_inner(cx, sp, elt)];
        }
        T_struct(tys)
      }
      ty::ty_opaque_closure_ptr(_) {
        T_opaque_cbox_ptr(cx)
      }
      ty::ty_constr(subt,_) {
        // FIXME: could be a constraint on ty_fn
          check non_ty_var(cx, subt);
          type_of_inner(cx, sp, subt)
      }
      _ {
        fail "type_of_inner not implemented for this kind of type";
      }
    };
    cx.lltypes.insert(t, llty);
    ret llty;
}

fn type_of_tag(cx: @crate_ctxt, sp: span, did: ast::def_id, t: ty::t)
    -> TypeRef {
    let degen = vec::len(*ty::tag_variants(cx.tcx, did)) == 1u;
    if check type_has_static_size(cx, t) {
        let size = static_size_of_tag(cx, sp, t);
        if !degen { T_tag(cx, size) }
        else if size == 0u { T_struct([T_tag_variant(cx)]) }
        else { T_array(T_i8(), size) }
    }
    else {
        if degen { T_struct([T_tag_variant(cx)]) }
        else { T_opaque_tag(cx) }
    }
}

fn type_of_ty_param_bounds_and_ty(lcx: @local_ctxt, sp: span,
                                 tpt: ty::ty_param_bounds_and_ty) -> TypeRef {
    let cx = lcx.ccx;
    let t = tpt.ty;
    alt ty::struct(cx.tcx, t) {
      ty::ty_fn(_) | ty::ty_native_fn(_, _) {
        ret type_of_fn_from_ty(cx, sp, t, *tpt.bounds);
      }
      _ {
        // fall through
      }
    }
    // FIXME: could have a precondition on tpt, but that
    // doesn't work right now because one predicate can't imply
    // another
    check (type_has_static_size(cx, t));
    type_of(cx, sp, t)
}

fn type_of_or_i8(bcx: @block_ctxt, typ: ty::t) -> TypeRef {
    let ccx = bcx_ccx(bcx);
    if check type_has_static_size(ccx, typ) {
        let sp = bcx.sp;
        type_of(ccx, sp, typ)
    } else { T_i8() }
}


// Name sanitation. LLVM will happily accept identifiers with weird names, but
// gas doesn't!
fn sanitize(s: str) -> str {
    let result = "";
    for c: u8 in s {
        if c == '@' as u8 {
            result += "boxed_";
        } else {
            if c == ',' as u8 {
                result += "_";
            } else {
                if c == '{' as u8 || c == '(' as u8 {
                    result += "_of_";
                } else {
                    if c != 10u8 && c != '}' as u8 && c != ')' as u8 &&
                           c != ' ' as u8 && c != '\t' as u8 && c != ';' as u8
                       {
                        let v = [c];
                        result += str::unsafe_from_bytes(v);
                    }
                }
            }
        }
    }
    ret result;
}


fn log_fn_time(ccx: @crate_ctxt, name: str, start: time::timeval,
               end: time::timeval) {
    let elapsed =
        1000 * (end.sec - start.sec as int) +
            ((end.usec as int) - (start.usec as int)) / 1000;
    *ccx.stats.fn_times += [{ident: name, time: elapsed}];
}


fn decl_fn(llmod: ModuleRef, name: str, cc: uint, llty: TypeRef) ->
    ValueRef {
    let llfn: ValueRef =
        str::as_buf(name, {|buf|
            llvm::LLVMGetOrInsertFunction(llmod, buf, llty) });
    llvm::LLVMSetFunctionCallConv(llfn, cc as c_uint);
    ret llfn;
}

fn decl_cdecl_fn(llmod: ModuleRef, name: str, llty: TypeRef) -> ValueRef {
    ret decl_fn(llmod, name, lib::llvm::LLVMCCallConv, llty);
}


// Only use this if you are going to actually define the function. It's
// not valid to simply declare a function as internal.
fn decl_internal_cdecl_fn(llmod: ModuleRef, name: str, llty: TypeRef) ->
   ValueRef {
    let llfn = decl_cdecl_fn(llmod, name, llty);
    llvm::LLVMSetLinkage(llfn,
                         lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    ret llfn;
}

fn get_extern_fn(externs: hashmap<str, ValueRef>, llmod: ModuleRef, name: str,
                 cc: uint, ty: TypeRef) -> ValueRef {
    if externs.contains_key(name) { ret externs.get(name); }
    let f = decl_fn(llmod, name, cc, ty);
    externs.insert(name, f);
    ret f;
}

fn get_extern_const(externs: hashmap<str, ValueRef>, llmod: ModuleRef,
                    name: str, ty: TypeRef) -> ValueRef {
    if externs.contains_key(name) { ret externs.get(name); }
    let c = str::as_buf(name, {|buf| llvm::LLVMAddGlobal(llmod, ty, buf) });
    externs.insert(name, c);
    ret c;
}

fn get_simple_extern_fn(cx: @block_ctxt,
                        externs: hashmap<str, ValueRef>,
                        llmod: ModuleRef,
                        name: str, n_args: int) -> ValueRef {
    let ccx = cx.fcx.lcx.ccx;
    let inputs = vec::init_elt::<TypeRef>(ccx.int_type, n_args as uint);
    let output = ccx.int_type;
    let t = T_fn(inputs, output);
    ret get_extern_fn(externs, llmod, name,
                      lib::llvm::LLVMCCallConv, t);
}

fn trans_native_call(cx: @block_ctxt, externs: hashmap<str, ValueRef>,
                     llmod: ModuleRef, name: str, args: [ValueRef]) ->
   ValueRef {
    let n: int = vec::len::<ValueRef>(args) as int;
    let llnative: ValueRef =
        get_simple_extern_fn(cx, externs, llmod, name, n);
    let call_args: [ValueRef] = [];
    for a: ValueRef in args {
        call_args += [ZExtOrBitCast(cx, a, bcx_ccx(cx).int_type)];
    }
    ret Call(cx, llnative, call_args);
}

fn trans_free_if_not_gc(cx: @block_ctxt, v: ValueRef) -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    if !ccx.sess.opts.do_gc {
        Call(cx, ccx.upcalls.free,
             [PointerCast(cx, v, T_ptr(T_i8())),
              C_int(bcx_ccx(cx), 0)]);
    }
    ret cx;
}

fn trans_shared_free(cx: @block_ctxt, v: ValueRef) -> @block_ctxt {
    Call(cx, bcx_ccx(cx).upcalls.shared_free,
         [PointerCast(cx, v, T_ptr(T_i8()))]);
    ret cx;
}

fn umax(cx: @block_ctxt, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = ICmp(cx, lib::llvm::LLVMIntULT, a, b);
    ret Select(cx, cond, b, a);
}

fn umin(cx: @block_ctxt, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = ICmp(cx, lib::llvm::LLVMIntULT, a, b);
    ret Select(cx, cond, a, b);
}

fn align_to(cx: @block_ctxt, off: ValueRef, align: ValueRef) -> ValueRef {
    let mask = Sub(cx, align, C_int(bcx_ccx(cx), 1));
    let bumped = Add(cx, off, mask);
    ret And(cx, bumped, Not(cx, mask));
}


// Returns the real size of the given type for the current target.
fn llsize_of_real(cx: @crate_ctxt, t: TypeRef) -> uint {
    ret llvm::LLVMStoreSizeOfType(cx.td.lltd, t) as uint;
}

// Returns the real alignment of the given type for the current target.
fn llalign_of_real(cx: @crate_ctxt, t: TypeRef) -> uint {
    ret llvm::LLVMPreferredAlignmentOfType(cx.td.lltd, t) as uint;
}

fn llsize_of(cx: @crate_ctxt, t: TypeRef) -> ValueRef {
    ret llvm::LLVMConstIntCast(lib::llvm::llvm::LLVMSizeOf(t), cx.int_type,
                               False);
}

fn llalign_of(cx: @crate_ctxt, t: TypeRef) -> ValueRef {
    ret llvm::LLVMConstIntCast(lib::llvm::llvm::LLVMAlignOf(t), cx.int_type,
                               False);
}

fn size_of(cx: @block_ctxt, t: ty::t) -> result {
    size_of_(cx, t)
}

fn size_of_(cx: @block_ctxt, t: ty::t) -> result {
    let ccx = bcx_ccx(cx);
    if check type_has_static_size(ccx, t) {
        let sp = cx.sp;
        rslt(cx, llsize_of(bcx_ccx(cx), type_of(ccx, sp, t)))
    } else { dynamic_size_of(cx, t) }
}

fn align_of(cx: @block_ctxt, t: ty::t) -> result {
    let ccx = bcx_ccx(cx);
    if check type_has_static_size(ccx, t) {
        let sp = cx.sp;
        rslt(cx, llalign_of(bcx_ccx(cx), type_of(ccx, sp, t)))
    } else { dynamic_align_of(cx, t) }
}

fn alloca(cx: @block_ctxt, t: TypeRef) -> ValueRef {
    if cx.unreachable { ret llvm::LLVMGetUndef(t); }
    ret Alloca(new_raw_block_ctxt(cx.fcx, cx.fcx.llstaticallocas), t);
}

fn dynastack_alloca(cx: @block_ctxt, t: TypeRef, n: ValueRef, ty: ty::t) ->
   ValueRef {
    if cx.unreachable { ret llvm::LLVMGetUndef(t); }
    let bcx = cx;
    let dy_cx = new_raw_block_ctxt(cx.fcx, cx.fcx.lldynamicallocas);
    alt bcx_fcx(cx).llobstacktoken {
      none. {
        bcx_fcx(cx).llobstacktoken =
            some(mk_obstack_token(bcx_ccx(cx), cx.fcx));
      }
      some(_) {/* no-op */ }
    }

    let dynastack_alloc = bcx_ccx(bcx).upcalls.dynastack_alloc;
    let llsz = Mul(dy_cx,
                   C_uint(bcx_ccx(bcx), llsize_of_real(bcx_ccx(bcx), t)),
                   n);

    let ti = none;
    let lltydesc = get_tydesc(cx, ty, false, ti).result.val;

    let llresult = Call(dy_cx, dynastack_alloc, [llsz, lltydesc]);
    ret PointerCast(dy_cx, llresult, T_ptr(t));
}

fn mk_obstack_token(ccx: @crate_ctxt, fcx: @fn_ctxt) ->
   ValueRef {
    let cx = new_raw_block_ctxt(fcx, fcx.lldynamicallocas);
    ret Call(cx, ccx.upcalls.dynastack_mark, []);
}


// Creates a simpler, size-equivalent type. The resulting type is guaranteed
// to have (a) the same size as the type that was passed in; (b) to be non-
// recursive. This is done by replacing all boxes in a type with boxed unit
// types.
fn simplify_type(ccx: @crate_ctxt, typ: ty::t) -> ty::t {
    fn simplifier(ccx: @crate_ctxt, typ: ty::t) -> ty::t {
        alt ty::struct(ccx.tcx, typ) {
          ty::ty_box(_) | ty::ty_iface(_, _) {
            ret ty::mk_imm_box(ccx.tcx, ty::mk_nil(ccx.tcx));
          }
          ty::ty_uniq(_) {
            ret ty::mk_imm_uniq(ccx.tcx, ty::mk_nil(ccx.tcx));
          }
          ty::ty_fn(_) {
            ret ty::mk_tup(ccx.tcx,
                           [ty::mk_imm_box(ccx.tcx, ty::mk_nil(ccx.tcx)),
                            ty::mk_imm_box(ccx.tcx, ty::mk_nil(ccx.tcx))]);
          }
          ty::ty_res(_, sub, tps) {
            let sub1 = ty::substitute_type_params(ccx.tcx, tps, sub);
            ret ty::mk_tup(ccx.tcx,
                           [ty::mk_int(ccx.tcx), simplify_type(ccx, sub1)]);
          }
          _ { ret typ; }
        }
    }
    ret ty::fold_ty(ccx.tcx, ty::fm_general(bind simplifier(ccx, _)), typ);
}


// Computes the size of the data part of a non-dynamically-sized tag.
fn static_size_of_tag(cx: @crate_ctxt, sp: span, t: ty::t)
    : type_has_static_size(cx, t) -> uint {
    if cx.tag_sizes.contains_key(t) { ret cx.tag_sizes.get(t); }
    alt ty::struct(cx.tcx, t) {
      ty::ty_tag(tid, subtys) {
        // Compute max(variant sizes).

        let max_size = 0u;
        let variants = ty::tag_variants(cx.tcx, tid);
        for variant: ty::variant_info in *variants {
            let tup_ty = simplify_type(cx, ty::mk_tup(cx.tcx, variant.args));
            // Perform any type parameter substitutions.

            tup_ty = ty::substitute_type_params(cx.tcx, subtys, tup_ty);
            // Here we possibly do a recursive call.

            // FIXME: Avoid this check. Since the parent has static
            // size, any field must as well. There should be a way to
            // express that with constrained types.
            check (type_has_static_size(cx, tup_ty));
            let this_size = llsize_of_real(cx, type_of(cx, sp, tup_ty));
            if max_size < this_size { max_size = this_size; }
        }
        cx.tag_sizes.insert(t, max_size);
        ret max_size;
      }
      _ {
        cx.tcx.sess.span_fatal(sp, "non-tag passed to static_size_of_tag()");
      }
    }
}

fn dynamic_size_of(cx: @block_ctxt, t: ty::t) -> result {
    fn align_elements(cx: @block_ctxt, elts: [ty::t]) -> result {
        //
        // C padding rules:
        //
        //
        //   - Pad after each element so that next element is aligned.
        //   - Pad after final structure member so that whole structure
        //     is aligned to max alignment of interior.
        //

        let off = C_int(bcx_ccx(cx), 0);
        let max_align = C_int(bcx_ccx(cx), 1);
        let bcx = cx;
        for e: ty::t in elts {
            let elt_align = align_of(bcx, e);
            bcx = elt_align.bcx;
            let elt_size = size_of(bcx, e);
            bcx = elt_size.bcx;
            let aligned_off = align_to(bcx, off, elt_align.val);
            off = Add(bcx, aligned_off, elt_size.val);
            max_align = umax(bcx, max_align, elt_align.val);
        }
        off = align_to(bcx, off, max_align);
        //off = alt mode {
        //  align_total. {
        //    align_to(bcx, off, max_align)
        //  }
        //  align_next(t) {
        //    let {bcx, val: align} = align_of(bcx, t);
        //    align_to(bcx, off, align)
        //  }
        //};
        ret rslt(bcx, off);
    }
    alt ty::struct(bcx_tcx(cx), t) {
      ty::ty_param(p, _) {
        let szptr = field_of_tydesc(cx, t, false, abi::tydesc_field_size);
        ret rslt(szptr.bcx, Load(szptr.bcx, szptr.val));
      }
      ty::ty_rec(flds) {
        let tys: [ty::t] = [];
        for f: ty::field in flds { tys += [f.mt.ty]; }
        ret align_elements(cx, tys);
      }
      ty::ty_tup(elts) {
        let tys = [];
        for tp in elts { tys += [tp]; }
        ret align_elements(cx, tys);
      }
      ty::ty_tag(tid, tps) {
        let bcx = cx;
        let ccx = bcx_ccx(bcx);
        // Compute max(variant sizes).

        let max_size: ValueRef = alloca(bcx, ccx.int_type);
        Store(bcx, C_int(ccx, 0), max_size);
        let variants = ty::tag_variants(bcx_tcx(bcx), tid);
        for variant: ty::variant_info in *variants {
            // Perform type substitution on the raw argument types.

            let raw_tys: [ty::t] = variant.args;
            let tys: [ty::t] = [];
            for raw_ty: ty::t in raw_tys {
                let t = ty::substitute_type_params(bcx_tcx(cx), tps, raw_ty);
                tys += [t];
            }
            let rslt = align_elements(bcx, tys);
            bcx = rslt.bcx;
            let this_size = rslt.val;
            let old_max_size = Load(bcx, max_size);
            Store(bcx, umax(bcx, this_size, old_max_size), max_size);
        }
        let max_size_val = Load(bcx, max_size);
        let total_size =
            if vec::len(*variants) != 1u {
                Add(bcx, max_size_val, llsize_of(ccx, ccx.int_type))
            } else { max_size_val };
        ret rslt(bcx, total_size);
      }
    }
}

fn dynamic_align_of(cx: @block_ctxt, t: ty::t) -> result {
// FIXME: Typestate constraint that shows this alt is
// exhaustive
    alt ty::struct(bcx_tcx(cx), t) {
      ty::ty_param(p, _) {
        let aptr = field_of_tydesc(cx, t, false, abi::tydesc_field_align);
        ret rslt(aptr.bcx, Load(aptr.bcx, aptr.val));
      }
      ty::ty_rec(flds) {
        let a = C_int(bcx_ccx(cx), 1);
        let bcx = cx;
        for f: ty::field in flds {
            let align = align_of(bcx, f.mt.ty);
            bcx = align.bcx;
            a = umax(bcx, a, align.val);
        }
        ret rslt(bcx, a);
      }
      ty::ty_tag(_, _) {
        ret rslt(cx, C_int(bcx_ccx(cx), 1)); // FIXME: stub
      }
      ty::ty_tup(elts) {
        let a = C_int(bcx_ccx(cx), 1);
        let bcx = cx;
        for e in elts {
            let align = align_of(bcx, e);
            bcx = align.bcx;
            a = umax(bcx, a, align.val);
        }
        ret rslt(bcx, a);
      }
    }
}

// Given a pointer p, returns a pointer sz(p) (i.e., inc'd by sz bytes).
// The type of the returned pointer is always i8*.  If you care about the
// return type, use bump_ptr().
fn ptr_offs(bcx: @block_ctxt, base: ValueRef, sz: ValueRef) -> ValueRef {
    let raw = PointerCast(bcx, base, T_ptr(T_i8()));
    GEP(bcx, raw, [sz])
}

// Increment a pointer by a given amount and then cast it to be a pointer
// to a given type.
fn bump_ptr(bcx: @block_ctxt, t: ty::t, base: ValueRef, sz: ValueRef) ->
   ValueRef {
    let ccx = bcx_ccx(bcx);
    let bumped = ptr_offs(bcx, base, sz);
    if check type_has_static_size(ccx, t) {
        let sp = bcx.sp;
        let typ = T_ptr(type_of(ccx, sp, t));
        PointerCast(bcx, bumped, typ)
    } else { bumped }
}

// GEP_tup_like is a pain to use if you always have to precede it with a
// check.
fn GEP_tup_like_1(cx: @block_ctxt, t: ty::t, base: ValueRef, ixs: [int])
    -> result {
    check type_is_tup_like(cx, t);
    ret GEP_tup_like(cx, t, base, ixs);
}

// Replacement for the LLVM 'GEP' instruction when field-indexing into a
// tuple-like structure (tup, rec) with a static index. This one is driven off
// ty::struct and knows what to do when it runs into a ty_param stuck in the
// middle of the thing it's GEP'ing into. Much like size_of and align_of,
// above.
fn GEP_tup_like(bcx: @block_ctxt, t: ty::t, base: ValueRef, ixs: [int])
    : type_is_tup_like(bcx, t) -> result {

    fn compute_off(bcx: @block_ctxt,
                   off: ValueRef,
                   t: ty::t,
                   ixs: [int],
                   n: uint) -> (@block_ctxt, ValueRef, ty::t) {
        if n == vec::len(ixs) {
            ret (bcx, off, t);
        }

        let tcx = bcx_tcx(bcx);
        let ix = ixs[n];
        let bcx = bcx, off = off;
        int::range(0, ix) {|i|
            let comp_t = ty::get_element_type(tcx, t, i as uint);
            let align = align_of(bcx, comp_t);
            bcx = align.bcx;
            off = align_to(bcx, off, align.val);
            let sz = size_of(bcx, comp_t);
            bcx = sz.bcx;
            off = Add(bcx, off, sz.val);
        }

        let comp_t = ty::get_element_type(tcx, t, ix as uint);
        let align = align_of(bcx, comp_t);
        bcx = align.bcx;
        off = align_to(bcx, off, align.val);

        be compute_off(bcx, off, comp_t, ixs, n+1u);
    }

    if !ty::type_has_dynamic_size(bcx_tcx(bcx), t) {
        ret rslt(bcx, GEPi(bcx, base, ixs));
    }

    #debug["GEP_tup_like(t=%s,base=%s,ixs=%?)",
           ty_to_str(bcx_tcx(bcx), t),
           val_str(bcx_ccx(bcx).tn, base),
           ixs];

    // We require that ixs start with 0 and we expect the input to be a
    // pointer to an instance of type t, so we can safely ignore ixs[0],
    // basically.
    assert ixs[0] == 0;

    let (bcx, off, tar_t) = {
        compute_off(bcx, C_int(bcx_ccx(bcx), 0), t, ixs, 1u)
    };
    ret rslt(bcx, bump_ptr(bcx, tar_t, base, off));
}


// Replacement for the LLVM 'GEP' instruction when field indexing into a tag.
// This function uses GEP_tup_like() above and automatically performs casts as
// appropriate. @llblobptr is the data part of a tag value; its actual type is
// meaningless, as it will be cast away.
fn GEP_tag(cx: @block_ctxt, llblobptr: ValueRef, tag_id: ast::def_id,
           variant_id: ast::def_id, ty_substs: [ty::t],
           ix: uint) : valid_variant_index(ix, cx, tag_id, variant_id) ->
   result {
    let variant = ty::tag_variant_with_id(bcx_tcx(cx), tag_id, variant_id);
    // Synthesize a tuple type so that GEP_tup_like() can work its magic.
    // Separately, store the type of the element we're interested in.

    let arg_tys = variant.args;

    let true_arg_tys: [ty::t] = [];
    for aty: ty::t in arg_tys {
            // Would be nice to have a way of stating the invariant
            // that ty_substs is valid for aty
        let arg_ty = ty::substitute_type_params(bcx_tcx(cx), ty_substs, aty);
        true_arg_tys += [arg_ty];
    }

    // We know that ix < len(variant.args) -- so
    // it's safe to do this. (Would be nice to have
    // typestate guarantee that a dynamic bounds check
    // error can't happen here, but that's in the future.)
    let elem_ty = true_arg_tys[ix];

    let tup_ty = ty::mk_tup(bcx_tcx(cx), true_arg_tys);
    // Cast the blob pointer to the appropriate type, if we need to (i.e. if
    // the blob pointer isn't dynamically sized).

    let llunionptr: ValueRef;
    let sp = cx.sp;
    let ccx = bcx_ccx(cx);
    if check type_has_static_size(ccx, tup_ty) {
        let llty = type_of(ccx, sp, tup_ty);
        llunionptr = TruncOrBitCast(cx, llblobptr, T_ptr(llty));
    } else { llunionptr = llblobptr; }

    // Do the GEP_tup_like().
    // Silly check -- postcondition on mk_tup?
    check type_is_tup_like(cx, tup_ty);
    let rs = GEP_tup_like(cx, tup_ty, llunionptr, [0, ix as int]);
    // Cast the result to the appropriate type, if necessary.

    let rs_ccx = bcx_ccx(rs.bcx);
    let val =
        if check type_has_static_size(rs_ccx, elem_ty) {
            let llelemty = type_of(rs_ccx, sp, elem_ty);
            PointerCast(rs.bcx, rs.val, T_ptr(llelemty))
        } else { rs.val };

    ret rslt(rs.bcx, val);
}

// trans_shared_malloc: expects a type indicating which pointer type we want
// and a size indicating how much space we want malloc'd.
fn trans_shared_malloc(cx: @block_ctxt, llptr_ty: TypeRef, llsize: ValueRef)
   -> result {
    // FIXME: need a table to collect tydesc globals.

    let tydesc = C_null(T_ptr(bcx_ccx(cx).tydesc_type));
    let rval =
        Call(cx, bcx_ccx(cx).upcalls.shared_malloc,
             [llsize, tydesc]);
    ret rslt(cx, PointerCast(cx, rval, llptr_ty));
}

// trans_malloc_boxed_raw: expects an unboxed type and returns a pointer to
// enough space for something of that type, along with space for a reference
// count; in other words, it allocates a box for something of that type.
fn trans_malloc_boxed_raw(cx: @block_ctxt, t: ty::t) -> result {
    let bcx = cx;

    // Synthesize a fake box type structurally so we have something
    // to measure the size of.

    // We synthesize two types here because we want both the type of the
    // pointer and the pointee.  boxed_body is the type that we measure the
    // size of; box_ptr is the type that's converted to a TypeRef and used as
    // the pointer cast target in trans_raw_malloc.

    // The mk_int here is the space being
    // reserved for the refcount.
    let boxed_body = ty::mk_tup(bcx_tcx(bcx), [ty::mk_int(bcx_tcx(cx)), t]);
    let box_ptr = ty::mk_imm_box(bcx_tcx(bcx), t);
    let r = size_of(cx, boxed_body);
    let llsz = r.val; bcx = r.bcx;

    // Grab the TypeRef type of box_ptr, because that's what trans_raw_malloc
    // wants.
    // FIXME: Could avoid this check with a postcondition on mk_imm_box?
    // (requires Issue #586)
    let ccx = bcx_ccx(bcx);
    let sp = bcx.sp;
    check (type_has_static_size(ccx, box_ptr));
    let llty = type_of(ccx, sp, box_ptr);

    let ti = none;
    let tydesc_result = get_tydesc(bcx, t, true, ti);
    let lltydesc = tydesc_result.result.val; bcx = tydesc_result.result.bcx;

    let rval = Call(cx, ccx.upcalls.malloc,
                    [llsz, lltydesc]);
    ret rslt(cx, PointerCast(cx, rval, llty));
}

// trans_malloc_boxed: usefully wraps trans_malloc_box_raw; allocates a box,
// initializes the reference count to 1, and pulls out the body and rc
fn trans_malloc_boxed(cx: @block_ctxt, t: ty::t) ->
   {bcx: @block_ctxt, box: ValueRef, body: ValueRef} {
    let res = trans_malloc_boxed_raw(cx, t);
    let box = res.val;
    let rc = GEPi(res.bcx, box, [0, abi::box_rc_field_refcnt]);
    Store(res.bcx, C_int(bcx_ccx(cx), 1), rc);
    let body = GEPi(res.bcx, box, [0, abi::box_rc_field_body]);
    ret {bcx: res.bcx, box: res.val, body: body};
}

// Type descriptor and type glue stuff

// Given a type and a field index into its corresponding type descriptor,
// returns an LLVM ValueRef of that field from the tydesc, generating the
// tydesc if necessary.
fn field_of_tydesc(cx: @block_ctxt, t: ty::t, escapes: bool, field: int) ->
   result {
    let ti = none::<@tydesc_info>;
    let tydesc = get_tydesc(cx, t, escapes, ti).result;
    ret rslt(tydesc.bcx,
             GEPi(tydesc.bcx, tydesc.val, [0, field]));
}

// Given a type containing ty params, build a vector containing a ValueRef for
// each of the ty params it uses (from the current frame) and a vector of the
// indices of the ty params present in the type. This is used solely for
// constructing derived tydescs.
fn linearize_ty_params(cx: @block_ctxt, t: ty::t) ->
   {params: [uint], descs: [ValueRef]} {
    let param_vals: [ValueRef] = [];
    let param_defs: [uint] = [];
    type rr =
        {cx: @block_ctxt, mutable vals: [ValueRef], mutable defs: [uint]};

    fn linearizer(r: @rr, t: ty::t) {
        alt ty::struct(bcx_tcx(r.cx), t) {
          ty::ty_param(pid, _) {
            let seen: bool = false;
            for d: uint in r.defs { if d == pid { seen = true; } }
            if !seen {
                r.vals += [r.cx.fcx.lltyparams[pid].desc];
                r.defs += [pid];
            }
          }
          _ { }
        }
    }
    let x = @{cx: cx, mutable vals: param_vals, mutable defs: param_defs};
    let f = bind linearizer(x, _);
    ty::walk_ty(bcx_tcx(cx), f, t);
    ret {params: x.defs, descs: x.vals};
}

fn trans_stack_local_derived_tydesc(cx: @block_ctxt, llsz: ValueRef,
                                    llalign: ValueRef, llroottydesc: ValueRef,
                                    llfirstparam: ValueRef, n_params: uint)
    -> ValueRef {
    let llmyroottydesc = alloca(cx, bcx_ccx(cx).tydesc_type);

    // By convention, desc 0 is the root descriptor.
    let llroottydesc = Load(cx, llroottydesc);
    Store(cx, llroottydesc, llmyroottydesc);

    // Store a pointer to the rest of the descriptors.
    let ccx = bcx_ccx(cx);
    store_inbounds(cx, llfirstparam, llmyroottydesc,
                   [0, abi::tydesc_field_first_param]);
    store_inbounds(cx, C_uint(ccx, n_params), llmyroottydesc,
                   [0, abi::tydesc_field_n_params]);
    store_inbounds(cx, llsz, llmyroottydesc,
                   [0, abi::tydesc_field_size]);
    store_inbounds(cx, llalign, llmyroottydesc,
                   [0, abi::tydesc_field_align]);
    // FIXME legacy field, can be dropped
    store_inbounds(cx, C_uint(ccx, 0u), llmyroottydesc,
                   [0, abi::tydesc_field_obj_params]);
    ret llmyroottydesc;
}

fn get_derived_tydesc(cx: @block_ctxt, t: ty::t, escapes: bool,
                      &static_ti: option::t<@tydesc_info>) -> result {
    alt cx.fcx.derived_tydescs.find(t) {
      some(info) {
        // If the tydesc escapes in this context, the cached derived
        // tydesc also has to be one that was marked as escaping.
        if !(escapes && !info.escapes) {
            ret rslt(cx, info.lltydesc);
        }
      }
      none. {/* fall through */ }
    }

    bcx_ccx(cx).stats.n_derived_tydescs += 1u;
    let bcx = new_raw_block_ctxt(cx.fcx, cx.fcx.llderivedtydescs);
    let tys = linearize_ty_params(bcx, t);
    let root_ti = get_static_tydesc(bcx, t, tys.params);
    static_ti = some::<@tydesc_info>(root_ti);
    lazily_emit_all_tydesc_glue(cx, static_ti);
    let root = root_ti.tydesc;
    let sz = size_of(bcx, t);
    bcx = sz.bcx;
    let align = align_of(bcx, t);
    bcx = align.bcx;

    // Store the captured type descriptors in an alloca if the caller isn't
    // promising to do so itself.
    let n_params = ty::count_ty_params(bcx_tcx(bcx), t);

    assert (n_params == vec::len::<uint>(tys.params));
    assert (n_params == vec::len::<ValueRef>(tys.descs));

    let llparamtydescs =
        alloca(bcx, T_array(T_ptr(bcx_ccx(bcx).tydesc_type), n_params + 1u));
    let i = 0;

    // If the type descriptor escapes, we need to add in the root as
    // the first parameter, because upcall_get_type_desc() expects it.
    if escapes {
        Store(bcx, root, GEPi(bcx, llparamtydescs, [0, 0]));
        i += 1;
    }

    for td: ValueRef in tys.descs {
        Store(bcx, td, GEPi(bcx, llparamtydescs, [0, i]));
        i += 1;
    }

    let llfirstparam =
        PointerCast(bcx, llparamtydescs,
                    T_ptr(T_ptr(bcx_ccx(bcx).tydesc_type)));

    let v;
    if escapes {
        let ccx = bcx_ccx(bcx);
        let td_val =
            Call(bcx, ccx.upcalls.get_type_desc,
                 [C_null(T_ptr(T_nil())), sz.val,
                  align.val, C_uint(ccx, 1u + n_params), llfirstparam,
                  C_uint(ccx, 0u)]);
        v = td_val;
    } else {
        v = trans_stack_local_derived_tydesc(bcx, sz.val, align.val, root,
                                             llfirstparam, n_params);
    }
    bcx.fcx.derived_tydescs.insert(t, {lltydesc: v, escapes: escapes});
    ret rslt(cx, v);
}

type get_tydesc_result = {kind: tydesc_kind, result: result};

fn get_tydesc(cx: @block_ctxt, t: ty::t, escapes: bool,
              &static_ti: option::t<@tydesc_info>)
   -> get_tydesc_result {

    // Is the supplied type a type param? If so, return the passed-in tydesc.
    alt ty::type_param(bcx_tcx(cx), t) {
      some(id) {
        if id < vec::len(cx.fcx.lltyparams) {
            ret {kind: tk_param,
                 result: rslt(cx, cx.fcx.lltyparams[id].desc)};
        } else {
            bcx_tcx(cx).sess.span_bug(cx.sp,
                                      "Unbound typaram in get_tydesc: " +
                                          "t = " +
                                          ty_to_str(bcx_tcx(cx), t) +
                                          " ty_param = " +
                                          uint::str(id));
        }
      }
      none. {/* fall through */ }
    }

    // Does it contain a type param? If so, generate a derived tydesc.
    if ty::type_contains_params(bcx_tcx(cx), t) {
        ret {kind: tk_derived,
             result: get_derived_tydesc(cx, t, escapes, static_ti)};
    }
    // Otherwise, generate a tydesc if necessary, and return it.
    let info = get_static_tydesc(cx, t, []);
    static_ti = some(info);
    ret {kind: tk_static, result: rslt(cx, info.tydesc)};
}

fn get_static_tydesc(cx: @block_ctxt, t: ty::t, ty_params: [uint])
    -> @tydesc_info {
    alt bcx_ccx(cx).tydescs.find(t) {
      some(info) { ret info; }
      none. {
        bcx_ccx(cx).stats.n_static_tydescs += 1u;
        let info = declare_tydesc(cx.fcx.lcx, cx.sp, t, ty_params);
        bcx_ccx(cx).tydescs.insert(t, info);
        ret info;
      }
    }
}

fn set_no_inline(f: ValueRef) {
    llvm::LLVMAddFunctionAttr(f,
                              lib::llvm::LLVMNoInlineAttribute as
                                  lib::llvm::llvm::Attribute,
                              0u as c_uint);
}

// Tell LLVM to emit the information necessary to unwind the stack for the
// function f.
fn set_uwtable(f: ValueRef) {
    llvm::LLVMAddFunctionAttr(f,
                              lib::llvm::LLVMUWTableAttribute as
                                  lib::llvm::llvm::Attribute,
                              0u as c_uint);
}

fn set_always_inline(f: ValueRef) {
    llvm::LLVMAddFunctionAttr(f,
                              lib::llvm::LLVMAlwaysInlineAttribute as
                                  lib::llvm::llvm::Attribute,
                              0u as c_uint);
}

fn set_custom_stack_growth_fn(f: ValueRef) {
    // TODO: Remove this hack to work around the lack of u64 in the FFI.
    llvm::LLVMAddFunctionAttr(f, 0 as lib::llvm::llvm::Attribute,
                              1u as c_uint);
}

fn set_glue_inlining(cx: @local_ctxt, f: ValueRef, t: ty::t) {
    if ty::type_is_structural(cx.ccx.tcx, t) {
        set_no_inline(f);
    } else { set_always_inline(f); }
}


// Generates the declaration for (but doesn't emit) a type descriptor.
fn declare_tydesc(cx: @local_ctxt, sp: span, t: ty::t, ty_params: [uint])
    -> @tydesc_info {
    log(debug, "+++ declare_tydesc " + ty_to_str(cx.ccx.tcx, t));
    let ccx = cx.ccx;
    let llsize;
    let llalign;
    if check type_has_static_size(ccx, t) {
        let llty = type_of(ccx, sp, t);
        llsize = llsize_of(ccx, llty);
        llalign = llalign_of(ccx, llty);
    } else {
        // These will be overwritten as the derived tydesc is generated, so
        // we create placeholder values.

        llsize = C_int(ccx, 0);
        llalign = C_int(ccx, 0);
    }
    let name;
    if cx.ccx.sess.opts.debuginfo {
        name = mangle_internal_name_by_type_only(cx.ccx, t, "tydesc");
        name = sanitize(name);
    } else { name = mangle_internal_name_by_seq(cx.ccx, "tydesc"); }
    let gvar =
        str::as_buf(name,
                    {|buf|
                        llvm::LLVMAddGlobal(ccx.llmod, ccx.tydesc_type, buf)
                    });
    let info =
        @{ty: t,
          tydesc: gvar,
          size: llsize,
          align: llalign,
          mutable take_glue: none::<ValueRef>,
          mutable drop_glue: none::<ValueRef>,
          mutable free_glue: none::<ValueRef>,
          mutable cmp_glue: none::<ValueRef>,
          ty_params: ty_params};
    log(debug, "--- declare_tydesc " + ty_to_str(cx.ccx.tcx, t));
    ret info;
}

type glue_helper = fn@(@block_ctxt, ValueRef, ty::t);

fn declare_generic_glue(cx: @local_ctxt, t: ty::t, llfnty: TypeRef, name: str)
   -> ValueRef {
    let name = name;
    let fn_nm;
    if cx.ccx.sess.opts.debuginfo {
        fn_nm = mangle_internal_name_by_type_only(cx.ccx, t, "glue_" + name);
        fn_nm = sanitize(fn_nm);
    } else { fn_nm = mangle_internal_name_by_seq(cx.ccx, "glue_" + name); }
    let llfn = decl_cdecl_fn(cx.ccx.llmod, fn_nm, llfnty);
    set_glue_inlining(cx, llfn, t);
    ret llfn;
}

// FIXME: was this causing the leak?
fn make_generic_glue_inner(cx: @local_ctxt, sp: span, t: ty::t,
                           llfn: ValueRef, helper: glue_helper,
                           ty_params: [uint]) -> ValueRef {
    let fcx = new_fn_ctxt(cx, sp, llfn);
    llvm::LLVMSetLinkage(llfn,
                         lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    cx.ccx.stats.n_glues_created += 1u;
    // Any nontrivial glue is with values passed *by alias*; this is a
    // requirement since in many contexts glue is invoked indirectly and
    // the caller has no idea if it's dealing with something that can be
    // passed by value.

    let ccx = cx.ccx;
    let llty =
        if check type_has_static_size(ccx, t) {
            T_ptr(type_of(ccx, sp, t))
        } else { T_ptr(T_i8()) };

    let ty_param_count = vec::len::<uint>(ty_params);
    let lltyparams = llvm::LLVMGetParam(llfn, 2u as c_uint);
    let load_env_bcx = new_raw_block_ctxt(fcx, fcx.llloadenv);
    let lltydescs = [mutable];
    let p = 0u;
    while p < ty_param_count {
        let llparam = GEPi(load_env_bcx, lltyparams, [p as int]);
        llparam = Load(load_env_bcx, llparam);
        vec::grow_set(lltydescs, ty_params[p], 0 as ValueRef, llparam);
        p += 1u;
    }

    fcx.lltyparams = vec::map_mut(lltydescs, {|d| {desc: d, dicts: none}});

    let bcx = new_top_block_ctxt(fcx);
    let lltop = bcx.llbb;
    let llrawptr0 = llvm::LLVMGetParam(llfn, 3u as c_uint);
    let llval0 = BitCast(bcx, llrawptr0, llty);
    helper(bcx, llval0, t);
    finish_fn(fcx, lltop);
    ret llfn;
}

fn make_generic_glue(cx: @local_ctxt, sp: span, t: ty::t, llfn: ValueRef,
                     helper: glue_helper, ty_params: [uint], name: str) ->
   ValueRef {
    if !cx.ccx.sess.opts.stats {
        ret make_generic_glue_inner(cx, sp, t, llfn, helper, ty_params);
    }

    let start = time::get_time();
    let llval = make_generic_glue_inner(cx, sp, t, llfn, helper, ty_params);
    let end = time::get_time();
    log_fn_time(cx.ccx, "glue " + name + " " + ty_to_short_str(cx.ccx.tcx, t),
                start, end);
    ret llval;
}

fn emit_tydescs(ccx: @crate_ctxt) {
    ccx.tydescs.items {|key, val|
        let glue_fn_ty = T_ptr(T_glue_fn(ccx));
        let cmp_fn_ty = T_ptr(T_cmp_glue_fn(ccx));
        let ti = val;
        let take_glue =
            alt ti.take_glue {
              none. { ccx.stats.n_null_glues += 1u; C_null(glue_fn_ty) }
              some(v) { ccx.stats.n_real_glues += 1u; v }
            };
        let drop_glue =
            alt ti.drop_glue {
              none. { ccx.stats.n_null_glues += 1u; C_null(glue_fn_ty) }
              some(v) { ccx.stats.n_real_glues += 1u; v }
            };
        let free_glue =
            alt ti.free_glue {
              none. { ccx.stats.n_null_glues += 1u; C_null(glue_fn_ty) }
              some(v) { ccx.stats.n_real_glues += 1u; v }
            };
        let cmp_glue =
            alt ti.cmp_glue {
              none. { ccx.stats.n_null_glues += 1u; C_null(cmp_fn_ty) }
              some(v) { ccx.stats.n_real_glues += 1u; v }
            };

        let shape = shape::shape_of(ccx, key, ti.ty_params);
        let shape_tables =
            llvm::LLVMConstPointerCast(ccx.shape_cx.llshapetables,
                                       T_ptr(T_i8()));

        let tydesc =
            C_named_struct(ccx.tydesc_type,
                           [C_null(T_ptr(T_ptr(ccx.tydesc_type))),
                            ti.size, // size
                            ti.align, // align
                            take_glue, // take_glue
                            drop_glue, // drop_glue
                            free_glue, // free_glue
                            C_null(T_ptr(T_i8())), // unused
                            C_null(glue_fn_ty), // sever_glue
                            C_null(glue_fn_ty), // mark_glue
                            C_null(glue_fn_ty), // unused
                            cmp_glue, // cmp_glue
                            C_shape(ccx, shape), // shape
                            shape_tables, // shape_tables
                            C_int(ccx, 0), // n_params
                            C_int(ccx, 0)]); // n_obj_params

        let gvar = ti.tydesc;
        llvm::LLVMSetInitializer(gvar, tydesc);
        llvm::LLVMSetGlobalConstant(gvar, True);
        llvm::LLVMSetLinkage(gvar,
                             lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    };
}

fn make_take_glue(cx: @block_ctxt, v: ValueRef, t: ty::t) {

    let bcx = cx;
    let tcx = bcx_tcx(cx);
    // NB: v is an *alias* of type t here, not a direct value.
    bcx = alt ty::struct(tcx, t) {
      ty::ty_box(_) | ty::ty_iface(_, _) {
        incr_refcnt_of_boxed(bcx, Load(bcx, v))
      }
      ty::ty_uniq(_) {
        check trans_uniq::type_is_unique_box(bcx, t);
        let r = trans_uniq::duplicate(bcx, Load(bcx, v), t);
        Store(r.bcx, r.val, v);
        r.bcx
      }
      ty::ty_vec(_) | ty::ty_str. {
        let r = tvec::duplicate(bcx, Load(bcx, v), t);
        Store(r.bcx, r.val, v);
        r.bcx
      }
      ty::ty_send_type. {
        // sendable type descriptors are basically unique pointers,
        // they must be cloned when copied:
        let r = Load(bcx, v);
        let s = Call(bcx, bcx_ccx(bcx).upcalls.create_shared_type_desc, [r]);
        Store(bcx, s, v);
        bcx
      }
      ty::ty_native_fn(_, _) | ty::ty_fn(_) {
        trans_closure::make_fn_glue(bcx, v, t, take_ty)
      }
      ty::ty_opaque_closure_ptr(ck) {
        trans_closure::make_opaque_cbox_take_glue(bcx, ck, v)
      }
      _ if ty::type_is_structural(bcx_tcx(bcx), t) {
        iter_structural_ty(bcx, v, t, take_ty)
      }
      _ { bcx }
    };

    build_return(bcx);
}

fn incr_refcnt_of_boxed(cx: @block_ctxt, box_ptr: ValueRef) -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    let rc_ptr =
        GEPi(cx, box_ptr, [0, abi::box_rc_field_refcnt]);
    let rc = Load(cx, rc_ptr);
    rc = Add(cx, rc, C_int(ccx, 1));
    Store(cx, rc, rc_ptr);
    ret cx;
}

fn free_box(bcx: @block_ctxt, v: ValueRef, t: ty::t) -> @block_ctxt {
    ret alt ty::struct(bcx_tcx(bcx), t) {
      ty::ty_box(body_mt) {
        let v = PointerCast(bcx, v, type_of_1(bcx, t));
        let body = GEPi(bcx, v, [0, abi::box_rc_field_body]);
        let bcx = drop_ty(bcx, body, body_mt.ty);
        trans_free_if_not_gc(bcx, v)
      }

      _ { fail "free_box invoked with non-box type"; }
    };
}

fn make_free_glue(bcx: @block_ctxt, v: ValueRef, t: ty::t) {
    // v is a pointer to the actual box component of the type here. The
    // ValueRef will have the wrong type here (make_generic_glue is casting
    // everything to a pointer to the type that the glue acts on).
    let bcx = alt ty::struct(bcx_tcx(bcx), t) {
      ty::ty_box(body_mt) {
        free_box(bcx, v, t)
      }
      ty::ty_uniq(content_mt) {
        check trans_uniq::type_is_unique_box(bcx, t);
        let v = PointerCast(bcx, v, type_of_1(bcx, t));
        trans_uniq::make_free_glue(bcx, v, t)
      }
      ty::ty_vec(_) | ty::ty_str. {
        tvec::make_free_glue(bcx, PointerCast(bcx, v, type_of_1(bcx, t)), t)
      }
      ty::ty_iface(_, _) {
        // Call through the box's own fields-drop glue first.
        // Then free the body.
        let ccx = bcx_ccx(bcx);
        let llbox_ty = T_opaque_iface_ptr(ccx);
        let b = PointerCast(bcx, v, llbox_ty);
        let body = GEPi(bcx, b, [0, abi::box_rc_field_body]);
        let tydescptr = GEPi(bcx, body, [0, 0]);
        let tydesc = Load(bcx, tydescptr);
        let ti = none;
        call_tydesc_glue_full(bcx, body, tydesc,
                              abi::tydesc_field_drop_glue, ti);
        trans_free_if_not_gc(bcx, b)
      }
      ty::ty_send_type. {
        // sendable type descriptors are basically unique pointers,
        // they must be freed.
        let ccx = bcx_ccx(bcx);
        let v = PointerCast(bcx, v, T_ptr(ccx.tydesc_type));
        Call(bcx, ccx.upcalls.free_shared_type_desc, [v]);
        bcx
      }
      ty::ty_native_fn(_, _) | ty::ty_fn(_) {
        trans_closure::make_fn_glue(bcx, v, t, free_ty)
      }
      ty::ty_opaque_closure_ptr(ck) {
        trans_closure::make_opaque_cbox_free_glue(bcx, ck, v)
      }
      _ { bcx }
    };
    build_return(bcx);
}

fn make_drop_glue(bcx: @block_ctxt, v0: ValueRef, t: ty::t) {
    // NB: v0 is an *alias* of type t here, not a direct value.
    let ccx = bcx_ccx(bcx);
    let bcx =
        alt ty::struct(ccx.tcx, t) {
          ty::ty_box(_) | ty::ty_iface(_, _) {
              decr_refcnt_maybe_free(bcx, Load(bcx, v0), t)
          }
          ty::ty_uniq(_) | ty::ty_vec(_) | ty::ty_str. | ty::ty_send_type. {
            free_ty(bcx, Load(bcx, v0), t)
          }
          ty::ty_res(did, inner, tps) {
            trans_res_drop(bcx, v0, did, inner, tps)
          }
          ty::ty_native_fn(_, _) | ty::ty_fn(_) {
            trans_closure::make_fn_glue(bcx, v0, t, drop_ty)
          }
          ty::ty_opaque_closure_ptr(ck) {
            trans_closure::make_opaque_cbox_drop_glue(bcx, ck, v0)
          }
          _ {
            if ty::type_needs_drop(ccx.tcx, t) &&
               ty::type_is_structural(ccx.tcx, t) {
                iter_structural_ty(bcx, v0, t, drop_ty)
            } else { bcx }
          }
        };
    build_return(bcx);
}

fn trans_res_drop(cx: @block_ctxt, rs: ValueRef, did: ast::def_id,
                  inner_t: ty::t, tps: [ty::t]) -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    let inner_t_s = ty::substitute_type_params(ccx.tcx, tps, inner_t);
    let tup_ty = ty::mk_tup(ccx.tcx, [ty::mk_int(ccx.tcx), inner_t_s]);
    let drop_cx = new_sub_block_ctxt(cx, "drop res");
    let next_cx = new_sub_block_ctxt(cx, "next");

    // Silly check
    check type_is_tup_like(cx, tup_ty);
    let drop_flag = GEP_tup_like(cx, tup_ty, rs, [0, 0]);
    let cx = drop_flag.bcx;
    let null_test = IsNull(cx, Load(cx, drop_flag.val));
    CondBr(cx, null_test, next_cx.llbb, drop_cx.llbb);
    cx = drop_cx;

    check type_is_tup_like(cx, tup_ty);
    let val = GEP_tup_like(cx, tup_ty, rs, [0, 1]);
    cx = val.bcx;
    // Find and call the actual destructor.
    let dtor_addr = trans_common::get_res_dtor(ccx, cx.sp, did, inner_t);
    let args = [cx.fcx.llretptr, null_env_ptr(cx)];
    for tp: ty::t in tps {
        let ti: option::t<@tydesc_info> = none;
        let td = get_tydesc(cx, tp, false, ti).result;
        args += [td.val];
        cx = td.bcx;
    }
    // Kludge to work around the fact that we know the precise type of the
    // value here, but the dtor expects a type that still has opaque pointers
    // for type variables.
    let val_llty = lib::llvm::fn_ty_param_tys
        (llvm::LLVMGetElementType
         (llvm::LLVMTypeOf(dtor_addr)))[vec::len(args)];
    let val_cast = BitCast(cx, val.val, val_llty);
    Call(cx, dtor_addr, args + [val_cast]);

    cx = drop_ty(cx, val.val, inner_t_s);
    // FIXME #1184: Resource flag is larger than necessary
    Store(cx, C_int(ccx, 0), drop_flag.val);
    Br(cx, next_cx.llbb);
    ret next_cx;
}

fn decr_refcnt_maybe_free(cx: @block_ctxt, box_ptr: ValueRef, t: ty::t)
    -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    let rc_adj_cx = new_sub_block_ctxt(cx, "rc--");
    let free_cx = new_sub_block_ctxt(cx, "free");
    let next_cx = new_sub_block_ctxt(cx, "next");
    let llbox_ty = T_opaque_iface_ptr(ccx);
    let box_ptr = PointerCast(cx, box_ptr, llbox_ty);
    let null_test = IsNull(cx, box_ptr);
    CondBr(cx, null_test, next_cx.llbb, rc_adj_cx.llbb);
    let rc_ptr =
        GEPi(rc_adj_cx, box_ptr, [0, abi::box_rc_field_refcnt]);
    let rc = Load(rc_adj_cx, rc_ptr);
    rc = Sub(rc_adj_cx, rc, C_int(ccx, 1));
    Store(rc_adj_cx, rc, rc_ptr);
    let zero_test = ICmp(rc_adj_cx, lib::llvm::LLVMIntEQ, C_int(ccx, 0), rc);
    CondBr(rc_adj_cx, zero_test, free_cx.llbb, next_cx.llbb);
    let free_cx = free_ty(free_cx, box_ptr, t);
    Br(free_cx, next_cx.llbb);
    ret next_cx;
}


// Structural comparison: a rather involved form of glue.
fn maybe_name_value(cx: @crate_ctxt, v: ValueRef, s: str) {
    if cx.sess.opts.save_temps {
        let _: () = str::as_buf(s, {|buf| llvm::LLVMSetValueName(v, buf) });
    }
}


// Used only for creating scalar comparison glue.
tag scalar_type { nil_type; signed_int; unsigned_int; floating_point; }


fn compare_scalar_types(cx: @block_ctxt, lhs: ValueRef, rhs: ValueRef,
                        t: ty::t, op: ast::binop) -> result {
    let f = bind compare_scalar_values(cx, lhs, rhs, _, op);

    alt ty::struct(bcx_tcx(cx), t) {
      ty::ty_nil. { ret rslt(cx, f(nil_type)); }
      ty::ty_bool. | ty::ty_ptr(_) { ret rslt(cx, f(unsigned_int)); }
      ty::ty_int(_) { ret rslt(cx, f(signed_int)); }
      ty::ty_uint(_) { ret rslt(cx, f(unsigned_int)); }
      ty::ty_float(_) { ret rslt(cx, f(floating_point)); }
      ty::ty_type. {
        ret rslt(trans_fail(cx, none,
                            "attempt to compare values of type type"),
                 C_nil());
      }
      ty::ty_native(_) {
        let cx = trans_fail(cx, none::<span>,
                            "attempt to compare values of type native");
        ret rslt(cx, C_nil());
      }
      _ {
        // Should never get here, because t is scalar.
        bcx_ccx(cx).sess.bug("non-scalar type passed to \
                                 compare_scalar_types");
      }
    }
}


// A helper function to do the actual comparison of scalar values.
fn compare_scalar_values(cx: @block_ctxt, lhs: ValueRef, rhs: ValueRef,
                         nt: scalar_type, op: ast::binop) -> ValueRef {
    alt nt {
      nil_type. {
        // We don't need to do actual comparisons for nil.
        // () == () holds but () < () does not.
        alt op {
          ast::eq. | ast::le. | ast::ge. { ret C_bool(true); }
          ast::ne. | ast::lt. | ast::gt. { ret C_bool(false); }
        }
      }
      floating_point. {
        let cmp = alt op {
          ast::eq. { lib::llvm::LLVMRealOEQ }
          ast::ne. { lib::llvm::LLVMRealUNE }
          ast::lt. { lib::llvm::LLVMRealOLT }
          ast::le. { lib::llvm::LLVMRealOLE }
          ast::gt. { lib::llvm::LLVMRealOGT }
          ast::ge. { lib::llvm::LLVMRealOGE }
        };
        ret FCmp(cx, cmp, lhs, rhs);
      }
      signed_int. {
        let cmp = alt op {
          ast::eq. { lib::llvm::LLVMIntEQ }
          ast::ne. { lib::llvm::LLVMIntNE }
          ast::lt. { lib::llvm::LLVMIntSLT }
          ast::le. { lib::llvm::LLVMIntSLE }
          ast::gt. { lib::llvm::LLVMIntSGT }
          ast::ge. { lib::llvm::LLVMIntSGE }
        };
        ret ICmp(cx, cmp, lhs, rhs);
      }
      unsigned_int. {
        let cmp = alt op {
          ast::eq. { lib::llvm::LLVMIntEQ }
          ast::ne. { lib::llvm::LLVMIntNE }
          ast::lt. { lib::llvm::LLVMIntULT }
          ast::le. { lib::llvm::LLVMIntULE }
          ast::gt. { lib::llvm::LLVMIntUGT }
          ast::ge. { lib::llvm::LLVMIntUGE }
        };
        ret ICmp(cx, cmp, lhs, rhs);
      }
    }
}

type val_pair_fn = fn@(@block_ctxt, ValueRef, ValueRef) -> @block_ctxt;
type val_and_ty_fn = fn@(@block_ctxt, ValueRef, ty::t) -> @block_ctxt;

fn load_inbounds(cx: @block_ctxt, p: ValueRef, idxs: [int]) -> ValueRef {
    ret Load(cx, GEPi(cx, p, idxs));
}

fn store_inbounds(cx: @block_ctxt, v: ValueRef, p: ValueRef,
                  idxs: [int]) {
    Store(cx, v, GEPi(cx, p, idxs));
}

// Iterates through the elements of a structural type.
fn iter_structural_ty(cx: @block_ctxt, av: ValueRef, t: ty::t,
                      f: val_and_ty_fn) -> @block_ctxt {
    fn iter_boxpp(cx: @block_ctxt, box_cell: ValueRef, f: val_and_ty_fn) ->
       @block_ctxt {
        let box_ptr = Load(cx, box_cell);
        let tnil = ty::mk_nil(bcx_tcx(cx));
        let tbox = ty::mk_imm_box(bcx_tcx(cx), tnil);
        let inner_cx = new_sub_block_ctxt(cx, "iter box");
        let next_cx = new_sub_block_ctxt(cx, "next");
        let null_test = IsNull(cx, box_ptr);
        CondBr(cx, null_test, next_cx.llbb, inner_cx.llbb);
        let inner_cx = f(inner_cx, box_cell, tbox);
        Br(inner_cx, next_cx.llbb);
        ret next_cx;
    }

    fn iter_variant(cx: @block_ctxt, a_tup: ValueRef,
                    variant: ty::variant_info, tps: [ty::t], tid: ast::def_id,
                    f: val_and_ty_fn) -> @block_ctxt {
        if vec::len::<ty::t>(variant.args) == 0u { ret cx; }
        let fn_ty = variant.ctor_ty;
        let ccx = bcx_ccx(cx);
        let cx = cx;
        alt ty::struct(ccx.tcx, fn_ty) {
          ty::ty_fn({inputs: args, _}) {
            let j = 0u;
            let v_id = variant.id;
            for a: ty::arg in args {
                check (valid_variant_index(j, cx, tid, v_id));
                let rslt = GEP_tag(cx, a_tup, tid, v_id, tps, j);
                let llfldp_a = rslt.val;
                cx = rslt.bcx;
                let ty_subst = ty::substitute_type_params(ccx.tcx, tps, a.ty);
                cx = f(cx, llfldp_a, ty_subst);
                j += 1u;
            }
          }
        }
        ret cx;
    }

    /*
    Typestate constraint that shows the unimpl case doesn't happen?
    */
    let cx = cx;
    alt ty::struct(bcx_tcx(cx), t) {
      ty::ty_rec(fields) {
        let i: int = 0;
        for fld: ty::field in fields {
            // Silly check
            check type_is_tup_like(cx, t);
            let {bcx: bcx, val: llfld_a} = GEP_tup_like(cx, t, av, [0, i]);
            cx = f(bcx, llfld_a, fld.mt.ty);
            i += 1;
        }
      }
      ty::ty_tup(args) {
        let i = 0;
        for arg in args {
            // Silly check
            check type_is_tup_like(cx, t);
            let {bcx: bcx, val: llfld_a} = GEP_tup_like(cx, t, av, [0, i]);
            cx = f(bcx, llfld_a, arg);
            i += 1;
        }
      }
      ty::ty_res(_, inner, tps) {
        let tcx = bcx_tcx(cx);
        let inner1 = ty::substitute_type_params(tcx, tps, inner);
        let inner_t_s = ty::substitute_type_params(tcx, tps, inner);
        let tup_t = ty::mk_tup(tcx, [ty::mk_int(tcx), inner_t_s]);
        // Silly check
        check type_is_tup_like(cx, tup_t);
        let {bcx: bcx, val: llfld_a} = GEP_tup_like(cx, tup_t, av, [0, 1]);
        ret f(bcx, llfld_a, inner1);
      }
      ty::ty_tag(tid, tps) {
        let variants = ty::tag_variants(bcx_tcx(cx), tid);
        let n_variants = vec::len(*variants);

        // Cast the tags to types we can GEP into.
        if n_variants == 1u {
            ret iter_variant(cx, av, variants[0], tps, tid, f);
        }

        let ccx = bcx_ccx(cx);
        let lltagty = T_opaque_tag_ptr(ccx);
        let av_tag = PointerCast(cx, av, lltagty);
        let lldiscrim_a_ptr = GEPi(cx, av_tag, [0, 0]);
        let llunion_a_ptr = GEPi(cx, av_tag, [0, 1]);
        let lldiscrim_a = Load(cx, lldiscrim_a_ptr);

        // NB: we must hit the discriminant first so that structural
        // comparison know not to proceed when the discriminants differ.
        cx = f(cx, lldiscrim_a_ptr, ty::mk_int(bcx_tcx(cx)));
        let unr_cx = new_sub_block_ctxt(cx, "tag-iter-unr");
        Unreachable(unr_cx);
        let llswitch = Switch(cx, lldiscrim_a, unr_cx.llbb, n_variants);
        let next_cx = new_sub_block_ctxt(cx, "tag-iter-next");
        for variant: ty::variant_info in *variants {
            let variant_cx =
                new_sub_block_ctxt(cx,
                                   "tag-iter-variant-" +
                                       int::to_str(variant.disr_val, 10u));
            AddCase(llswitch, C_int(ccx, variant.disr_val), variant_cx.llbb);
            variant_cx =
                iter_variant(variant_cx, llunion_a_ptr, variant, tps, tid, f);
            Br(variant_cx, next_cx.llbb);
        }
        ret next_cx;
      }
      _ { bcx_ccx(cx).sess.unimpl("type in iter_structural_ty"); }
    }
    ret cx;
}

fn lazily_emit_all_tydesc_glue(cx: @block_ctxt,
                               static_ti: option::t<@tydesc_info>) {
    lazily_emit_tydesc_glue(cx, abi::tydesc_field_take_glue, static_ti);
    lazily_emit_tydesc_glue(cx, abi::tydesc_field_drop_glue, static_ti);
    lazily_emit_tydesc_glue(cx, abi::tydesc_field_free_glue, static_ti);
    lazily_emit_tydesc_glue(cx, abi::tydesc_field_cmp_glue, static_ti);
}

fn lazily_emit_all_generic_info_tydesc_glues(cx: @block_ctxt,
                                             gi: generic_info) {
    for ti: option::t<@tydesc_info> in gi.static_tis {
        lazily_emit_all_tydesc_glue(cx, ti);
    }
}

fn lazily_emit_tydesc_glue(cx: @block_ctxt, field: int,
                           static_ti: option::t<@tydesc_info>) {
    alt static_ti {
      none. { }
      some(ti) {
        if field == abi::tydesc_field_take_glue {
            alt ti.take_glue {
              some(_) { }
              none. {
                #debug("+++ lazily_emit_tydesc_glue TAKE %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
                let lcx = cx.fcx.lcx;
                let glue_fn =
                    declare_generic_glue(lcx, ti.ty, T_glue_fn(lcx.ccx),
                                         "take");
                ti.take_glue = some::<ValueRef>(glue_fn);
                make_generic_glue(lcx, cx.sp, ti.ty, glue_fn,
                                  make_take_glue,
                                  ti.ty_params, "take");
                #debug("--- lazily_emit_tydesc_glue TAKE %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
              }
            }
        } else if field == abi::tydesc_field_drop_glue {
            alt ti.drop_glue {
              some(_) { }
              none. {
                #debug("+++ lazily_emit_tydesc_glue DROP %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
                let lcx = cx.fcx.lcx;
                let glue_fn =
                    declare_generic_glue(lcx, ti.ty, T_glue_fn(lcx.ccx),
                                         "drop");
                ti.drop_glue = some::<ValueRef>(glue_fn);
                make_generic_glue(lcx, cx.sp, ti.ty, glue_fn,
                                  make_drop_glue,
                                  ti.ty_params, "drop");
                #debug("--- lazily_emit_tydesc_glue DROP %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
              }
            }
        } else if field == abi::tydesc_field_free_glue {
            alt ti.free_glue {
              some(_) { }
              none. {
                #debug("+++ lazily_emit_tydesc_glue FREE %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
                let lcx = cx.fcx.lcx;
                let glue_fn =
                    declare_generic_glue(lcx, ti.ty, T_glue_fn(lcx.ccx),
                                         "free");
                ti.free_glue = some::<ValueRef>(glue_fn);
                make_generic_glue(lcx, cx.sp, ti.ty, glue_fn,
                                  make_free_glue,
                                  ti.ty_params, "free");
                #debug("--- lazily_emit_tydesc_glue FREE %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
              }
            }
        } else if field == abi::tydesc_field_cmp_glue {
            alt ti.cmp_glue {
              some(_) { }
              none. {
                #debug("+++ lazily_emit_tydesc_glue CMP %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
                ti.cmp_glue = some(bcx_ccx(cx).upcalls.cmp_type);
                #debug("--- lazily_emit_tydesc_glue CMP %s",
                       ty_to_str(bcx_tcx(cx), ti.ty));
              }
            }
        }
      }
    }
}

fn call_tydesc_glue_full(cx: @block_ctxt, v: ValueRef, tydesc: ValueRef,
                         field: int, static_ti: option::t<@tydesc_info>) {
    lazily_emit_tydesc_glue(cx, field, static_ti);

    let static_glue_fn = none;
    alt static_ti {
      none. {/* no-op */ }
      some(sti) {
        if field == abi::tydesc_field_take_glue {
            static_glue_fn = sti.take_glue;
        } else if field == abi::tydesc_field_drop_glue {
            static_glue_fn = sti.drop_glue;
        } else if field == abi::tydesc_field_free_glue {
            static_glue_fn = sti.free_glue;
        }
      }
    }

    let llrawptr = PointerCast(cx, v, T_ptr(T_i8()));
    let lltydescs =
        GEPi(cx, tydesc, [0, abi::tydesc_field_first_param]);
    lltydescs = Load(cx, lltydescs);

    let llfn;
    alt static_glue_fn {
      none. {
        let llfnptr = GEPi(cx, tydesc, [0, field]);
        llfn = Load(cx, llfnptr);
      }
      some(sgf) { llfn = sgf; }
    }

    Call(cx, llfn, [C_null(T_ptr(T_nil())), C_null(T_ptr(T_nil())),
                    lltydescs, llrawptr]);
}

fn call_tydesc_glue(cx: @block_ctxt, v: ValueRef, t: ty::t, field: int) ->
   @block_ctxt {
    let ti: option::t<@tydesc_info> = none::<@tydesc_info>;
    let {bcx: bcx, val: td} = get_tydesc(cx, t, false, ti).result;
    call_tydesc_glue_full(bcx, v, td, field, ti);
    ret bcx;
}

fn call_cmp_glue(cx: @block_ctxt, lhs: ValueRef, rhs: ValueRef, t: ty::t,
                 llop: ValueRef) -> result {
    // We can't use call_tydesc_glue_full() and friends here because compare
    // glue has a special signature.

    let bcx = cx;

    let r = spill_if_immediate(bcx, lhs, t);
    let lllhs = r.val;
    bcx = r.bcx;
    r = spill_if_immediate(bcx, rhs, t);
    let llrhs = r.val;
    bcx = r.bcx;

    let llrawlhsptr = BitCast(bcx, lllhs, T_ptr(T_i8()));
    let llrawrhsptr = BitCast(bcx, llrhs, T_ptr(T_i8()));
    let ti = none::<@tydesc_info>;
    r = get_tydesc(bcx, t, false, ti).result;
    let lltydesc = r.val;
    bcx = r.bcx;
    lazily_emit_tydesc_glue(bcx, abi::tydesc_field_cmp_glue, ti);
    let lltydescs =
        GEPi(bcx, lltydesc, [0, abi::tydesc_field_first_param]);
    lltydescs = Load(bcx, lltydescs);

    let llfn;
    alt ti {
      none. {
        let llfnptr =
            GEPi(bcx, lltydesc, [0, abi::tydesc_field_cmp_glue]);
        llfn = Load(bcx, llfnptr);
      }
      some(sti) { llfn = option::get(sti.cmp_glue); }
    }

    let llcmpresultptr = alloca(bcx, T_i1());
    Call(bcx, llfn, [llcmpresultptr, lltydesc, lltydescs,
                     llrawlhsptr, llrawrhsptr, llop]);
    ret rslt(bcx, Load(bcx, llcmpresultptr));
}

fn take_ty(cx: @block_ctxt, v: ValueRef, t: ty::t) -> @block_ctxt {
    if ty::type_needs_drop(bcx_tcx(cx), t) {
        ret call_tydesc_glue(cx, v, t, abi::tydesc_field_take_glue);
    }
    ret cx;
}

fn drop_ty(cx: @block_ctxt, v: ValueRef, t: ty::t) -> @block_ctxt {
    if ty::type_needs_drop(bcx_tcx(cx), t) {
        ret call_tydesc_glue(cx, v, t, abi::tydesc_field_drop_glue);
    }
    ret cx;
}

fn drop_ty_immediate(bcx: @block_ctxt, v: ValueRef, t: ty::t) -> @block_ctxt {
    alt ty::struct(bcx_tcx(bcx), t) {
      ty::ty_uniq(_) | ty::ty_vec(_) | ty::ty_str. { free_ty(bcx, v, t) }
      ty::ty_box(_) | ty::ty_iface(_, _) { decr_refcnt_maybe_free(bcx, v, t) }
    }
}

fn take_ty_immediate(bcx: @block_ctxt, v: ValueRef, t: ty::t) -> result {
    alt ty::struct(bcx_tcx(bcx), t) {
      ty::ty_box(_) | ty::ty_iface(_, _) {
        rslt(incr_refcnt_of_boxed(bcx, v), v)
      }
      ty::ty_uniq(_) {
        check trans_uniq::type_is_unique_box(bcx, t);
        trans_uniq::duplicate(bcx, v, t)
      }
      ty::ty_str. | ty::ty_vec(_) { tvec::duplicate(bcx, v, t) }
      _ { rslt(bcx, v) }
    }
}

fn free_ty(cx: @block_ctxt, v: ValueRef, t: ty::t) -> @block_ctxt {
    if ty::type_needs_drop(bcx_tcx(cx), t) {
        ret call_tydesc_glue(cx, v, t, abi::tydesc_field_free_glue);
    }
    ret cx;
}

fn call_memmove(cx: @block_ctxt, dst: ValueRef, src: ValueRef,
                n_bytes: ValueRef) -> result {
    // TODO: Provide LLVM with better alignment information when the alignment
    // is statically known (it must be nothing more than a constant int, or
    // LLVM complains -- not even a constant element of a tydesc works).

    let ccx = bcx_ccx(cx);
    let key = alt ccx.sess.targ_cfg.arch {
      session::arch_x86. | session::arch_arm. { "llvm.memmove.p0i8.p0i8.i32" }
      session::arch_x86_64. { "llvm.memmove.p0i8.p0i8.i64" }
    };
    let i = ccx.intrinsics;
    assert (i.contains_key(key));
    let memmove = i.get(key);
    let src_ptr = PointerCast(cx, src, T_ptr(T_i8()));
    let dst_ptr = PointerCast(cx, dst, T_ptr(T_i8()));
    // FIXME #1184: Resource flag is larger than necessary
    let size = IntCast(cx, n_bytes, ccx.int_type);
    let align = C_i32(1i32);
    let volatile = C_bool(false);
    let ret_val = Call(cx, memmove, [dst_ptr, src_ptr, size,
                                     align, volatile]);
    ret rslt(cx, ret_val);
}

fn memmove_ty(bcx: @block_ctxt, dst: ValueRef, src: ValueRef, t: ty::t) ->
    @block_ctxt {
    let ccx = bcx_ccx(bcx);
    if check type_has_static_size(ccx, t) {
        if ty::type_is_structural(bcx_tcx(bcx), t) {
            let sp = bcx.sp;
            let llsz = llsize_of(ccx, type_of(ccx, sp, t));
            ret call_memmove(bcx, dst, src, llsz).bcx;
        }
        Store(bcx, Load(bcx, src), dst);
        ret bcx;
    }

    let {bcx, val: llsz} = size_of(bcx, t);
    ret call_memmove(bcx, dst, src, llsz).bcx;
}

tag copy_action { INIT; DROP_EXISTING; }

// These are the types that are passed by pointer.
fn type_is_structural_or_param(tcx: ty::ctxt, t: ty::t) -> bool {
    if ty::type_is_structural(tcx, t) { ret true; }
    alt ty::struct(tcx, t) {
      ty::ty_param(_, _) { ret true; }
      _ { ret false; }
    }
}

fn copy_val(cx: @block_ctxt, action: copy_action, dst: ValueRef,
            src: ValueRef, t: ty::t) -> @block_ctxt {
    if action == DROP_EXISTING &&
        (type_is_structural_or_param(bcx_tcx(cx), t) ||
         ty::type_is_unique(bcx_tcx(cx), t)) {
        let do_copy_cx = new_sub_block_ctxt(cx, "do_copy");
        let next_cx = new_sub_block_ctxt(cx, "next");
        let dstcmp = load_if_immediate(cx, dst, t);
        let self_assigning =
            ICmp(cx, lib::llvm::LLVMIntNE,
                 PointerCast(cx, dstcmp, val_ty(src)), src);
        CondBr(cx, self_assigning, do_copy_cx.llbb, next_cx.llbb);
        do_copy_cx = copy_val_no_check(do_copy_cx, action, dst, src, t);
        Br(do_copy_cx, next_cx.llbb);
        ret next_cx;
    }
    ret copy_val_no_check(cx, action, dst, src, t);
}

fn copy_val_no_check(bcx: @block_ctxt, action: copy_action, dst: ValueRef,
                     src: ValueRef, t: ty::t) -> @block_ctxt {
    let ccx = bcx_ccx(bcx), bcx = bcx;
    if ty::type_is_scalar(ccx.tcx, t) || ty::type_is_native(ccx.tcx, t) {
        Store(bcx, src, dst);
        ret bcx;
    }
    if ty::type_is_nil(ccx.tcx, t) || ty::type_is_bot(ccx.tcx, t) { ret bcx; }
    if ty::type_is_boxed(ccx.tcx, t) || ty::type_is_vec(ccx.tcx, t) ||
       ty::type_is_unique_box(ccx.tcx, t) {
        if action == DROP_EXISTING { bcx = drop_ty(bcx, dst, t); }
        Store(bcx, src, dst);
        ret take_ty(bcx, dst, t);
    }
    if type_is_structural_or_param(ccx.tcx, t) {
        if action == DROP_EXISTING { bcx = drop_ty(bcx, dst, t); }
        bcx = memmove_ty(bcx, dst, src, t);
        ret take_ty(bcx, dst, t);
    }
    ccx.sess.bug("unexpected type in trans::copy_val_no_check: " +
                     ty_to_str(ccx.tcx, t));
}


// This works like copy_val, except that it deinitializes the source.
// Since it needs to zero out the source, src also needs to be an lval.
// FIXME: We always zero out the source. Ideally we would detect the
// case where a variable is always deinitialized by block exit and thus
// doesn't need to be dropped.
fn move_val(cx: @block_ctxt, action: copy_action, dst: ValueRef,
            src: lval_result, t: ty::t) -> @block_ctxt {
    let src_val = src.val;
    let tcx = bcx_tcx(cx), cx = cx;
    if ty::type_is_scalar(tcx, t) || ty::type_is_native(tcx, t) {
        if src.kind == owned { src_val = Load(cx, src_val); }
        Store(cx, src_val, dst);
        ret cx;
    } else if ty::type_is_nil(tcx, t) || ty::type_is_bot(tcx, t) {
        ret cx;
    } else if ty::type_is_boxed(tcx, t) || ty::type_is_unique(tcx, t) {
        if src.kind == owned { src_val = Load(cx, src_val); }
        if action == DROP_EXISTING { cx = drop_ty(cx, dst, t); }
        Store(cx, src_val, dst);
        if src.kind == owned { ret zero_alloca(cx, src.val, t); }
        // If we're here, it must be a temporary.
        revoke_clean(cx, src_val);
        ret cx;
    } else if type_is_structural_or_param(tcx, t) {
        if action == DROP_EXISTING { cx = drop_ty(cx, dst, t); }
        cx = memmove_ty(cx, dst, src_val, t);
        if src.kind == owned { ret zero_alloca(cx, src_val, t); }
        // If we're here, it must be a temporary.
        revoke_clean(cx, src_val);
        ret cx;
    }
    /* FIXME: suggests a type constraint */
    bcx_ccx(cx).sess.bug("unexpected type in trans::move_val: " +
                             ty_to_str(tcx, t));
}

fn store_temp_expr(cx: @block_ctxt, action: copy_action, dst: ValueRef,
                   src: lval_result, t: ty::t, last_use: bool)
    -> @block_ctxt {
    // Lvals in memory are not temporaries. Copy them.
    if src.kind != temporary && !last_use {
        let v = src.kind == owned ? load_if_immediate(cx, src.val, t)
                                  : src.val;
        ret copy_val(cx, action, dst, v, t);
    }
    ret move_val(cx, action, dst, src, t);
}

fn trans_crate_lit(cx: @crate_ctxt, lit: ast::lit) -> ValueRef {
    alt lit.node {
      ast::lit_int(i, t) { C_integral(T_int_ty(cx, t), i as u64, True) }
      ast::lit_uint(u, t) { C_integral(T_uint_ty(cx, t), u, False) }
      ast::lit_float(fs, t) { C_floating(fs, T_float_ty(cx, t)) }
      ast::lit_bool(b) { C_bool(b) }
      ast::lit_nil. { C_nil() }
      ast::lit_str(s) {
        cx.sess.span_unimpl(lit.span, "unique string in this context");
      }
    }
}

fn trans_lit(cx: @block_ctxt, lit: ast::lit, dest: dest) -> @block_ctxt {
    if dest == ignore { ret cx; }
    alt lit.node {
      ast::lit_str(s) { ret tvec::trans_str(cx, s, dest); }
      _ {
        ret store_in_dest(cx, trans_crate_lit(bcx_ccx(cx), lit), dest);
      }
    }
}


// Converts an annotation to a type
fn node_id_type(cx: @crate_ctxt, id: ast::node_id) -> ty::t {
    ret ty::node_id_to_monotype(cx.tcx, id);
}

fn node_type(cx: @crate_ctxt, sp: span, id: ast::node_id) -> TypeRef {
    let ty = node_id_type(cx, id);
    // How to make this a precondition?
    // FIXME (again, would require a predicate that implies
    // another predicate)
    check (type_has_static_size(cx, ty));
    type_of(cx, sp, ty)
}

fn trans_unary(bcx: @block_ctxt, op: ast::unop, e: @ast::expr,
               id: ast::node_id, dest: dest) -> @block_ctxt {
    if dest == ignore { ret trans_expr(bcx, e, ignore); }
    let e_ty = ty::expr_ty(bcx_tcx(bcx), e);
    alt op {
      ast::not. {
        let {bcx, val} = trans_temp_expr(bcx, e);
        ret store_in_dest(bcx, Not(bcx, val), dest);
      }
      ast::neg. {
        let {bcx, val} = trans_temp_expr(bcx, e);
        let neg = if ty::type_is_fp(bcx_tcx(bcx), e_ty) {
            FNeg(bcx, val)
        } else { Neg(bcx, val) };
        ret store_in_dest(bcx, neg, dest);
      }
      ast::box(_) {
        let {bcx, box, body} = trans_malloc_boxed(bcx, e_ty);
        add_clean_free(bcx, box, false);
        // Cast the body type to the type of the value. This is needed to
        // make tags work, since tags have a different LLVM type depending
        // on whether they're boxed or not.
        let ccx = bcx_ccx(bcx);
        if check type_has_static_size(ccx, e_ty) {
            let e_sp = e.span;
            let llety = T_ptr(type_of(ccx, e_sp, e_ty));
            body = PointerCast(bcx, body, llety);
        }
        bcx = trans_expr_save_in(bcx, e, body);
        revoke_clean(bcx, box);
        ret store_in_dest(bcx, box, dest);
      }
      ast::uniq(_) {
        ret trans_uniq::trans_uniq(bcx, e, id, dest);
      }
      ast::deref. {
        bcx_ccx(bcx).sess.bug("deref expressions should have been \
                               translated using trans_lval(), not \
                               trans_unary()");
      }
    }
}

fn trans_compare(cx: @block_ctxt, op: ast::binop, lhs: ValueRef,
                 _lhs_t: ty::t, rhs: ValueRef, rhs_t: ty::t) -> result {
    if ty::type_is_scalar(bcx_tcx(cx), rhs_t) {
      let rs = compare_scalar_types(cx, lhs, rhs, rhs_t, op);
      ret rslt(rs.bcx, rs.val);
    }

    // Determine the operation we need.
    let llop;
    alt op {
      ast::eq. | ast::ne. { llop = C_u8(abi::cmp_glue_op_eq); }
      ast::lt. | ast::ge. { llop = C_u8(abi::cmp_glue_op_lt); }
      ast::le. | ast::gt. { llop = C_u8(abi::cmp_glue_op_le); }
    }

    let rs = call_cmp_glue(cx, lhs, rhs, rhs_t, llop);

    // Invert the result if necessary.
    alt op {
      ast::eq. | ast::lt. | ast::le. { ret rslt(rs.bcx, rs.val); }
      ast::ne. | ast::ge. | ast::gt. {
        ret rslt(rs.bcx, Not(rs.bcx, rs.val));
      }
    }
}

// Important to get types for both lhs and rhs, because one might be _|_
// and the other not.
fn trans_eager_binop(cx: @block_ctxt, op: ast::binop, lhs: ValueRef,
                     lhs_t: ty::t, rhs: ValueRef, rhs_t: ty::t, dest: dest)
    -> @block_ctxt {
    if dest == ignore { ret cx; }
    let intype = lhs_t;
    if ty::type_is_bot(bcx_tcx(cx), intype) { intype = rhs_t; }
    let is_float = ty::type_is_fp(bcx_tcx(cx), intype);

    if op == ast::add && ty::type_is_sequence(bcx_tcx(cx), intype) {
        ret tvec::trans_add(cx, intype, lhs, rhs, dest);
    }
    let cx = cx, val = alt op {
      ast::add. {
        if is_float { FAdd(cx, lhs, rhs) }
        else { Add(cx, lhs, rhs) }
      }
      ast::subtract. {
        if is_float { FSub(cx, lhs, rhs) }
        else { Sub(cx, lhs, rhs) }
      }
      ast::mul. {
        if is_float { FMul(cx, lhs, rhs) }
        else { Mul(cx, lhs, rhs) }
      }
      ast::div. {
        if is_float { FDiv(cx, lhs, rhs) }
        else if ty::type_is_signed(bcx_tcx(cx), intype) {
            SDiv(cx, lhs, rhs)
        } else { UDiv(cx, lhs, rhs) }
      }
      ast::rem. {
        if is_float { FRem(cx, lhs, rhs) }
        else if ty::type_is_signed(bcx_tcx(cx), intype) {
            SRem(cx, lhs, rhs)
        } else { URem(cx, lhs, rhs) }
      }
      ast::bitor. { Or(cx, lhs, rhs) }
      ast::bitand. { And(cx, lhs, rhs) }
      ast::bitxor. { Xor(cx, lhs, rhs) }
      ast::lsl. { Shl(cx, lhs, rhs) }
      ast::lsr. { LShr(cx, lhs, rhs) }
      ast::asr. { AShr(cx, lhs, rhs) }
      _ {
        let cmpr = trans_compare(cx, op, lhs, lhs_t, rhs, rhs_t);
        cx = cmpr.bcx;
        cmpr.val
      }
    };
    ret store_in_dest(cx, val, dest);
}

fn trans_assign_op(bcx: @block_ctxt, op: ast::binop, dst: @ast::expr,
                   src: @ast::expr) -> @block_ctxt {
    let tcx = bcx_tcx(bcx);
    let t = ty::expr_ty(tcx, src);
    let lhs_res = trans_lval(bcx, dst);
    assert (lhs_res.kind == owned);
    // Special case for `+= [x]`
    alt ty::struct(tcx, t) {
      ty::ty_vec(_) {
        alt src.node {
          ast::expr_vec(args, _) {
            ret tvec::trans_append_literal(lhs_res.bcx,
                                           lhs_res.val, t, args);
          }
          _ { }
        }
      }
      _ { }
    }
    let {bcx, val: rhs_val} = trans_temp_expr(lhs_res.bcx, src);
    if ty::type_is_sequence(tcx, t) {
        alt op {
          ast::add. {
            ret tvec::trans_append(bcx, t, lhs_res.val, rhs_val);
          }
          _ { }
        }
    }
    ret trans_eager_binop(bcx, op, Load(bcx, lhs_res.val), t, rhs_val, t,
                          save_in(lhs_res.val));
}

fn autoderef(cx: @block_ctxt, v: ValueRef, t: ty::t) -> result_t {
    let v1: ValueRef = v;
    let t1: ty::t = t;
    let ccx = bcx_ccx(cx);
    let sp = cx.sp;
    while true {
        alt ty::struct(ccx.tcx, t1) {
          ty::ty_box(mt) {
            let body = GEPi(cx, v1, [0, abi::box_rc_field_body]);
            t1 = mt.ty;

            // Since we're changing levels of box indirection, we may have
            // to cast this pointer, since statically-sized tag types have
            // different types depending on whether they're behind a box
            // or not.
            if check type_has_static_size(ccx, t1) {
                let llty = type_of(ccx, sp, t1);
                v1 = PointerCast(cx, body, T_ptr(llty));
            } else { v1 = body; }
          }
          ty::ty_uniq(_) {
            check trans_uniq::type_is_unique_box(cx, t1);
            let derefed = trans_uniq::autoderef(cx, v1, t1);
            t1 = derefed.t;
            v1 = derefed.v;
          }
          ty::ty_res(did, inner, tps) {
            t1 = ty::substitute_type_params(ccx.tcx, tps, inner);
            v1 = GEPi(cx, v1, [0, 1]);
          }
          ty::ty_tag(did, tps) {
            let variants = ty::tag_variants(ccx.tcx, did);
            if vec::len(*variants) != 1u ||
                   vec::len(variants[0].args) != 1u {
                break;
            }
            t1 =
                ty::substitute_type_params(ccx.tcx, tps, variants[0].args[0]);
            if check type_has_static_size(ccx, t1) {
                v1 = PointerCast(cx, v1, T_ptr(type_of(ccx, sp, t1)));
            } else { } // FIXME: typestate hack
          }
          _ { break; }
        }
        v1 = load_if_immediate(cx, v1, t1);
    }
    ret {bcx: cx, val: v1, ty: t1};
}

fn trans_lazy_binop(bcx: @block_ctxt, op: ast::binop, a: @ast::expr,
                    b: @ast::expr, dest: dest) -> @block_ctxt {
    let is_and = alt op { ast::and. { true } ast::or. { false } };
    let lhs_res = trans_temp_expr(bcx, a);
    if lhs_res.bcx.unreachable { ret lhs_res.bcx; }
    let rhs_cx = new_scope_block_ctxt(lhs_res.bcx, "rhs");
    let rhs_res = trans_temp_expr(rhs_cx, b);

    let lhs_past_cx = new_scope_block_ctxt(lhs_res.bcx, "lhs");
    // The following line ensures that any cleanups for rhs
    // are done within the block for rhs. This is necessary
    // because and/or are lazy. So the rhs may never execute,
    // and the cleanups can't be pushed into later code.
    let rhs_bcx = trans_block_cleanups(rhs_res.bcx, rhs_cx);
    if is_and {
        CondBr(lhs_res.bcx, lhs_res.val, rhs_cx.llbb, lhs_past_cx.llbb);
    } else {
        CondBr(lhs_res.bcx, lhs_res.val, lhs_past_cx.llbb, rhs_cx.llbb);
    }

    let join_cx = new_sub_block_ctxt(bcx, "join");
    Br(lhs_past_cx, join_cx.llbb);
    if rhs_bcx.unreachable {
        ret store_in_dest(join_cx, C_bool(!is_and), dest);
    }
    Br(rhs_bcx, join_cx.llbb);
    let phi = Phi(join_cx, T_bool(), [C_bool(!is_and), rhs_res.val],
                  [lhs_past_cx.llbb, rhs_bcx.llbb]);
    ret store_in_dest(join_cx, phi, dest);
}

fn trans_binary(cx: @block_ctxt, op: ast::binop, a: @ast::expr, b: @ast::expr,
                dest: dest) -> @block_ctxt {
    // First couple cases are lazy:
    alt op {
      ast::and. | ast::or. {
        ret trans_lazy_binop(cx, op, a, b, dest);
      }
      _ {
        // Remaining cases are eager:
        let lhs = trans_temp_expr(cx, a);
        let rhs = trans_temp_expr(lhs.bcx, b);
        ret trans_eager_binop(rhs.bcx, op, lhs.val,
                              ty::expr_ty(bcx_tcx(cx), a), rhs.val,
                              ty::expr_ty(bcx_tcx(cx), b), dest);
      }
    }
}

tag dest {
    by_val(@mutable ValueRef);
    save_in(ValueRef);
    ignore;
}

fn empty_dest_cell() -> @mutable ValueRef {
    ret @mutable llvm::LLVMGetUndef(T_nil());
}

fn dup_for_join(dest: dest) -> dest {
    alt dest {
      by_val(_) { by_val(empty_dest_cell()) }
      _ { dest }
    }
}

fn join_returns(parent_cx: @block_ctxt, in_cxs: [@block_ctxt],
                in_ds: [dest], out_dest: dest) -> @block_ctxt {
    let out = new_sub_block_ctxt(parent_cx, "join");
    let reachable = false, i = 0u, phi = none;
    for cx in in_cxs {
        if !cx.unreachable {
            Br(cx, out.llbb);
            reachable = true;
            alt in_ds[i] {
              by_val(cell) {
                if option::is_none(phi) {
                    phi = some(EmptyPhi(out, val_ty(*cell)));
                }
                AddIncomingToPhi(option::get(phi), *cell, cx.llbb);
              }
              _ {}
            }
        }
        i += 1u;
    }
    if !reachable {
        Unreachable(out);
    } else {
        alt out_dest {
          by_val(cell) { *cell = option::get(phi); }
          _ {}
        }
    }
    ret out;
}

// Used to put an immediate value in a dest.
fn store_in_dest(bcx: @block_ctxt, val: ValueRef, dest: dest) -> @block_ctxt {
    alt dest {
      ignore. {}
      by_val(cell) { *cell = val; }
      save_in(addr) { Store(bcx, val, addr); }
    }
    ret bcx;
}

fn get_dest_addr(dest: dest) -> ValueRef {
    alt dest { save_in(a) { a } }
}

fn trans_if(cx: @block_ctxt, cond: @ast::expr, thn: ast::blk,
            els: option::t<@ast::expr>, dest: dest)
    -> @block_ctxt {
    let {bcx, val: cond_val} = trans_temp_expr(cx, cond);

    let then_dest = dup_for_join(dest);
    let else_dest = dup_for_join(dest);
    let then_cx = new_scope_block_ctxt(bcx, "then");
    let else_cx = new_scope_block_ctxt(bcx, "else");
    CondBr(bcx, cond_val, then_cx.llbb, else_cx.llbb);
    then_cx = trans_block_dps(then_cx, thn, then_dest);
    // Calling trans_block directly instead of trans_expr
    // because trans_expr will create another scope block
    // context for the block, but we've already got the
    // 'else' context
    alt els {
      some(elexpr) {
        alt elexpr.node {
          ast::expr_if(_, _, _) {
            let elseif_blk = ast_util::block_from_expr(elexpr);
            else_cx = trans_block_dps(else_cx, elseif_blk, else_dest);
          }
          ast::expr_block(blk) {
            else_cx = trans_block_dps(else_cx, blk, else_dest);
          }
        }
      }
      _ {}
    }
    ret join_returns(cx, [then_cx, else_cx], [then_dest, else_dest], dest);
}

fn trans_for(cx: @block_ctxt, local: @ast::local, seq: @ast::expr,
             body: ast::blk) -> @block_ctxt {
    fn inner(bcx: @block_ctxt, local: @ast::local, curr: ValueRef, t: ty::t,
             body: ast::blk, outer_next_cx: @block_ctxt) -> @block_ctxt {
        let next_cx = new_sub_block_ctxt(bcx, "next");
        let scope_cx =
            new_loop_scope_block_ctxt(bcx, option::some(next_cx),
                                      outer_next_cx, "for loop scope");
        Br(bcx, scope_cx.llbb);
        let curr = PointerCast(bcx, curr, T_ptr(type_of_or_i8(bcx, t)));
        let bcx = trans_alt::bind_irrefutable_pat(scope_cx, local.node.pat,
                                                  curr, false);
        bcx = trans_block_dps(bcx, body, ignore);
        Br(bcx, next_cx.llbb);
        ret next_cx;
    }
    let ccx = bcx_ccx(cx);
    let next_cx = new_sub_block_ctxt(cx, "next");
    let seq_ty = ty::expr_ty(bcx_tcx(cx), seq);
    let {bcx: bcx, val: seq} = trans_temp_expr(cx, seq);
    let seq = PointerCast(bcx, seq, T_ptr(ccx.opaque_vec_type));
    let fill = tvec::get_fill(bcx, seq);
    if ty::type_is_str(bcx_tcx(bcx), seq_ty) {
        fill = Sub(bcx, fill, C_int(ccx, 1));
    }
    let bcx = tvec::iter_vec_raw(bcx, seq, seq_ty, fill,
                                 bind inner(_, local, _, _, body, next_cx));
    Br(bcx, next_cx.llbb);
    ret next_cx;
}

fn trans_while(cx: @block_ctxt, cond: @ast::expr, body: ast::blk)
    -> @block_ctxt {
    let next_cx = new_sub_block_ctxt(cx, "while next");
    let cond_cx =
        new_loop_scope_block_ctxt(cx, option::none::<@block_ctxt>, next_cx,
                                  "while cond");
    let body_cx = new_scope_block_ctxt(cond_cx, "while loop body");
    let body_end = trans_block(body_cx, body);
    let cond_res = trans_temp_expr(cond_cx, cond);
    Br(body_end, cond_cx.llbb);
    let cond_bcx = trans_block_cleanups(cond_res.bcx, cond_cx);
    CondBr(cond_bcx, cond_res.val, body_cx.llbb, next_cx.llbb);
    Br(cx, cond_cx.llbb);
    ret next_cx;
}

fn trans_do_while(cx: @block_ctxt, body: ast::blk, cond: @ast::expr) ->
    @block_ctxt {
    let next_cx = new_sub_block_ctxt(cx, "next");
    let body_cx =
        new_loop_scope_block_ctxt(cx, option::none::<@block_ctxt>, next_cx,
                                  "do-while loop body");
    let body_end = trans_block(body_cx, body);
    let cond_cx = new_scope_block_ctxt(body_cx, "do-while cond");
    Br(body_end, cond_cx.llbb);
    let cond_res = trans_temp_expr(cond_cx, cond);
    let cond_bcx = trans_block_cleanups(cond_res.bcx, cond_cx);
    CondBr(cond_bcx, cond_res.val, body_cx.llbb, next_cx.llbb);
    Br(cx, body_cx.llbb);
    ret next_cx;
}

type generic_info = {
    item_type: ty::t,
    static_tis: [option::t<@tydesc_info>],
    tydescs: [ValueRef],
    param_bounds: @[ty::param_bounds],
    origins: option::t<typeck::dict_res>
};

tag lval_kind {
    temporary; //< Temporary value passed by value if of immediate type
    owned;     //< Non-temporary value passed by pointer
    owned_imm; //< Non-temporary value passed by value
}
type local_var_result = {val: ValueRef, kind: lval_kind};
type lval_result = {bcx: @block_ctxt, val: ValueRef, kind: lval_kind};
tag callee_env {
    null_env;
    is_closure;
    self_env(ValueRef);
    dict_env(ValueRef, ValueRef);
}
type lval_maybe_callee = {bcx: @block_ctxt,
                          val: ValueRef,
                          kind: lval_kind,
                          env: callee_env,
                          generic: option::t<generic_info>};

fn null_env_ptr(bcx: @block_ctxt) -> ValueRef {
    C_null(T_opaque_cbox_ptr(bcx_ccx(bcx)))
}

fn lval_from_local_var(bcx: @block_ctxt, r: local_var_result) -> lval_result {
    ret { bcx: bcx, val: r.val, kind: r.kind };
}

fn lval_owned(bcx: @block_ctxt, val: ValueRef) -> lval_result {
    ret {bcx: bcx, val: val, kind: owned};
}
fn lval_temp(bcx: @block_ctxt, val: ValueRef) -> lval_result {
    ret {bcx: bcx, val: val, kind: temporary};
}

fn lval_no_env(bcx: @block_ctxt, val: ValueRef, kind: lval_kind)
    -> lval_maybe_callee {
    ret {bcx: bcx, val: val, kind: kind, env: is_closure, generic: none};
}

fn trans_external_path(cx: @block_ctxt, did: ast::def_id,
                       tpt: ty::ty_param_bounds_and_ty) -> ValueRef {
    let lcx = cx.fcx.lcx;
    let name = csearch::get_symbol(lcx.ccx.sess.cstore, did);
    ret get_extern_const(lcx.ccx.externs, lcx.ccx.llmod, name,
                         type_of_ty_param_bounds_and_ty(lcx, cx.sp, tpt));
}

fn lval_static_fn(bcx: @block_ctxt, fn_id: ast::def_id, id: ast::node_id)
    -> lval_maybe_callee {
    let ccx = bcx_ccx(bcx);
    let tpt = ty::lookup_item_type(ccx.tcx, fn_id);
    let val = if fn_id.crate == ast::local_crate {
        // Internal reference.
        assert (ccx.item_ids.contains_key(fn_id.node));
        ccx.item_ids.get(fn_id.node)
    } else {
        // External reference.
        trans_external_path(bcx, fn_id, tpt)
    };
    let tys = ty::node_id_to_type_params(ccx.tcx, id);
    let gen = none, bcx = bcx;
    if vec::len(tys) != 0u {
        let tydescs = [], tis = [];
        for t in tys {
            // TODO: Doesn't always escape.
            let ti = none;
            let td = get_tydesc(bcx, t, true, ti).result;
            tis += [ti];
            bcx = td.bcx;
            tydescs += [td.val];
        }
        gen = some({item_type: tpt.ty,
                    static_tis: tis,
                    tydescs: tydescs,
                    param_bounds: tpt.bounds,
                    origins: ccx.dict_map.find(id)});
    }
    ret {bcx: bcx, val: val, kind: owned, env: null_env, generic: gen};
}

fn lookup_discriminant(lcx: @local_ctxt, vid: ast::def_id) -> ValueRef {
    let ccx = lcx.ccx;
    alt ccx.discrims.find(vid) {
      none. {
        // It's an external discriminant that we haven't seen yet.
        assert (vid.crate != ast::local_crate);
        let sym = csearch::get_symbol(lcx.ccx.sess.cstore, vid);
        let gvar =
            str::as_buf(sym,
                        {|buf|
                            llvm::LLVMAddGlobal(ccx.llmod, ccx.int_type, buf)
                        });
        llvm::LLVMSetLinkage(gvar,
                             lib::llvm::LLVMExternalLinkage as llvm::Linkage);
        llvm::LLVMSetGlobalConstant(gvar, True);
        lcx.ccx.discrims.insert(vid, gvar);
        ret gvar;
      }
      some(llval) { ret llval; }
    }
}

fn trans_local_var(cx: @block_ctxt, def: ast::def) -> local_var_result {
    fn take_local(table: hashmap<ast::node_id, local_val>,
                  id: ast::node_id) -> local_var_result {
        alt table.find(id) {
          some(local_mem(v)) { {val: v, kind: owned} }
          some(local_imm(v)) { {val: v, kind: owned_imm} }
          r { fail("take_local: internal error"); }
        }
    }
    alt def {
      ast::def_upvar(did, _, _) {
        assert (cx.fcx.llupvars.contains_key(did.node));
        ret { val: cx.fcx.llupvars.get(did.node), kind: owned };
      }
      ast::def_arg(did, _) {
        assert (cx.fcx.llargs.contains_key(did.node));
        ret take_local(cx.fcx.llargs, did.node);
      }
      ast::def_local(did, _) | ast::def_binding(did) {
        assert (cx.fcx.lllocals.contains_key(did.node));
        ret take_local(cx.fcx.lllocals, did.node);
      }
      ast::def_self(did) {
        let slf = option::get(cx.fcx.llself);
        let ptr = PointerCast(cx, slf.v, T_ptr(type_of_or_i8(cx, slf.t)));
        ret {val: ptr, kind: owned};
      }
      _ {
        bcx_ccx(cx).sess.span_unimpl
            (cx.sp, "unsupported def type in trans_local_def");
      }
    }
}

fn trans_path(cx: @block_ctxt, p: @ast::path, id: ast::node_id)
    -> lval_maybe_callee {
    ret trans_var(cx, p.span, bcx_tcx(cx).def_map.get(id), id);
}

fn trans_var(cx: @block_ctxt, sp: span, def: ast::def, id: ast::node_id)
    -> lval_maybe_callee {
    let ccx = bcx_ccx(cx);
    alt def {
      ast::def_fn(did, _) | ast::def_native_fn(did, _) {
        ret lval_static_fn(cx, did, id);
      }
      ast::def_variant(tid, vid) {
        if vec::len(ty::tag_variant_with_id(ccx.tcx, tid, vid).args) > 0u {
            // N-ary variant.
            ret lval_static_fn(cx, vid, id);
        } else {
            // Nullary variant.
            let tag_ty = node_id_type(ccx, id);
            let alloc_result = alloc_ty(cx, tag_ty);
            let lltagblob = alloc_result.val;
            let lltagty = type_of_tag(ccx, sp, tid, tag_ty);
            let bcx = alloc_result.bcx;
            let lltagptr = PointerCast(bcx, lltagblob, T_ptr(lltagty));
            let lldiscrimptr = GEPi(bcx, lltagptr, [0, 0]);
            let lldiscrim_gv = lookup_discriminant(bcx.fcx.lcx, vid);
            let lldiscrim = Load(bcx, lldiscrim_gv);
            Store(bcx, lldiscrim, lldiscrimptr);
            ret lval_no_env(bcx, lltagptr, temporary);
        }
      }
      ast::def_const(did) {
        if did.crate == ast::local_crate {
            assert (ccx.consts.contains_key(did.node));
            ret lval_no_env(cx, ccx.consts.get(did.node), owned);
        } else {
            let tp = ty::node_id_to_monotype(ccx.tcx, id);
            let val = trans_external_path(cx, did, {bounds: @[], ty: tp});
            ret lval_no_env(cx, load_if_immediate(cx, val, tp), owned_imm);
        }
      }
      _ {
        let loc = trans_local_var(cx, def);
        ret lval_no_env(cx, loc.val, loc.kind);
      }
    }
}

fn trans_rec_field(bcx: @block_ctxt, base: @ast::expr,
                   field: ast::ident) -> lval_result {
    let {bcx, val} = trans_temp_expr(bcx, base);
    let {bcx, val, ty} = autoderef(bcx, val, ty::expr_ty(bcx_tcx(bcx), base));
    let fields = alt ty::struct(bcx_tcx(bcx), ty) { ty::ty_rec(fs) { fs } };
    let ix = option::get(ty::field_idx(field, fields));
    // Silly check
    check type_is_tup_like(bcx, ty);
    let {bcx, val} = GEP_tup_like(bcx, ty, val, [0, ix as int]);
    ret {bcx: bcx, val: val, kind: owned};
}

fn trans_index(cx: @block_ctxt, sp: span, base: @ast::expr, idx: @ast::expr,
               id: ast::node_id) -> lval_result {
    // Is this an interior vector?
    let base_ty = ty::expr_ty(bcx_tcx(cx), base);
    let exp = trans_temp_expr(cx, base);
    let lv = autoderef(exp.bcx, exp.val, base_ty);
    let ix = trans_temp_expr(lv.bcx, idx);
    let v = lv.val;
    let bcx = ix.bcx;
    let ccx = bcx_ccx(cx);

    // Cast to an LLVM integer. Rust is less strict than LLVM in this regard.
    let ix_val;
    let ix_size = llsize_of_real(bcx_ccx(cx), val_ty(ix.val));
    let int_size = llsize_of_real(bcx_ccx(cx), ccx.int_type);
    if ix_size < int_size {
        ix_val = ZExt(bcx, ix.val, ccx.int_type);
    } else if ix_size > int_size {
        ix_val = Trunc(bcx, ix.val, ccx.int_type);
    } else { ix_val = ix.val; }

    let unit_ty = node_id_type(bcx_ccx(cx), id);
    let unit_sz = size_of(bcx, unit_ty);
    bcx = unit_sz.bcx;
    maybe_name_value(bcx_ccx(cx), unit_sz.val, "unit_sz");
    let scaled_ix = Mul(bcx, ix_val, unit_sz.val);
    maybe_name_value(bcx_ccx(cx), scaled_ix, "scaled_ix");
    let lim = tvec::get_fill(bcx, v);
    let body = tvec::get_dataptr(bcx, v, type_of_or_i8(bcx, unit_ty));
    let bounds_check = ICmp(bcx, lib::llvm::LLVMIntULT, scaled_ix, lim);
    let fail_cx = new_sub_block_ctxt(bcx, "fail");
    let next_cx = new_sub_block_ctxt(bcx, "next");
    let ncx = bcx_ccx(next_cx);
    CondBr(bcx, bounds_check, next_cx.llbb, fail_cx.llbb);
    // fail: bad bounds check.

    trans_fail(fail_cx, some::<span>(sp), "bounds check");
    let elt =
        if check type_has_static_size(ncx, unit_ty) {
            let elt_1 = GEP(next_cx, body, [ix_val]);
            let llunitty = type_of(ncx, sp, unit_ty);
            PointerCast(next_cx, elt_1, T_ptr(llunitty))
        } else {
            body = PointerCast(next_cx, body, T_ptr(T_i8()));
            GEP(next_cx, body, [scaled_ix])
        };

    ret lval_owned(next_cx, elt);
}

fn expr_is_lval(bcx: @block_ctxt, e: @ast::expr) -> bool {
    let ccx = bcx_ccx(bcx);
    ty::expr_is_lval(ccx.method_map, e)
}

fn trans_callee(bcx: @block_ctxt, e: @ast::expr) -> lval_maybe_callee {
    alt e.node {
      ast::expr_path(p) { ret trans_path(bcx, p, e.id); }
      ast::expr_field(base, ident, _) {
        // Lval means this is a record field, so not a method
        if !expr_is_lval(bcx, e) {
            alt bcx_ccx(bcx).method_map.find(e.id) {
              some(typeck::method_static(did)) { // An impl method
                ret trans_impl::trans_static_callee(bcx, e, base, did);
              }
              some(typeck::method_param(iid, off, p, b)) {
                ret trans_impl::trans_param_callee(
                    bcx, e, base, iid, off, p, b);
              }
              some(typeck::method_iface(off)) {
                ret trans_impl::trans_iface_callee(bcx, e, base, off);
              }
            }
        }
      }
      _ {}
    }
    let lv = trans_temp_lval(bcx, e);
    ret lval_no_env(lv.bcx, lv.val, lv.kind);
}

// Use this when you know you are compiling an lval.
// The additional bool returned indicates whether it's mem (that is
// represented as an alloca or heap, hence needs a 'load' to be used as an
// immediate).
fn trans_lval(cx: @block_ctxt, e: @ast::expr) -> lval_result {
    alt e.node {
      ast::expr_path(p) {
        let v = trans_path(cx, p, e.id);
        ret lval_maybe_callee_to_lval(v, ty::expr_ty(bcx_tcx(cx), e));
      }
      ast::expr_field(base, ident, _) {
        ret trans_rec_field(cx, base, ident);
      }
      ast::expr_index(base, idx) {
        ret trans_index(cx, e.span, base, idx, e.id);
      }
      ast::expr_unary(ast::deref., base) {
        let ccx = bcx_ccx(cx);
        let sub = trans_temp_expr(cx, base);
        let t = ty::expr_ty(ccx.tcx, base);
        let val =
            alt ty::struct(ccx.tcx, t) {
              ty::ty_box(_) {
                GEPi(sub.bcx, sub.val, [0, abi::box_rc_field_body])
              }
              ty::ty_res(_, _, _) {
                GEPi(sub.bcx, sub.val, [0, 1])
              }
              ty::ty_tag(_, _) {
                let ety = ty::expr_ty(ccx.tcx, e);
                let sp = e.span;
                let ellty =
                    if check type_has_static_size(ccx, ety) {
                        T_ptr(type_of(ccx, sp, ety))
                    } else { T_typaram_ptr(ccx.tn) };
                PointerCast(sub.bcx, sub.val, ellty)
              }
              ty::ty_ptr(_) | ty::ty_uniq(_) { sub.val }
            };
        ret lval_owned(sub.bcx, val);
      }
      // This is a by-ref returning call. Regular calls are not lval
      ast::expr_call(f, args, _) {
        let cell = empty_dest_cell();
        let bcx = trans_call(cx, f, args, e.id, by_val(cell));
        ret lval_owned(bcx, *cell);
      }
      _ { bcx_ccx(cx).sess.span_bug(e.span, "non-lval in trans_lval"); }
    }
}

fn maybe_add_env(bcx: @block_ctxt, c: lval_maybe_callee)
    -> (lval_kind, ValueRef) {
    alt c.env {
      is_closure. { (c.kind, c.val) }
      self_env(_) | dict_env(_, _) {
        fail "Taking the value of a method does not work yet (issue #435)";
      }
      null_env. {
        let llfnty = llvm::LLVMGetElementType(val_ty(c.val));
        (temporary, create_real_fn_pair(bcx, llfnty, c.val,
                                        null_env_ptr(bcx)))
      }
    }
}

fn lval_maybe_callee_to_lval(c: lval_maybe_callee, ty: ty::t) -> lval_result {
    alt c.generic {
      some(gi) {
        let n_args = vec::len(ty::ty_fn_args(bcx_tcx(c.bcx), ty));
        let args = vec::init_elt(none::<@ast::expr>, n_args);
        let space = alloc_ty(c.bcx, ty);
        let bcx = trans_closure::trans_bind_1(space.bcx, ty, c, args, ty,
                                              save_in(space.val));
        add_clean_temp(bcx, space.val, ty);
        ret {bcx: bcx, val: space.val, kind: temporary};
      }
      none. {
        let (kind, val) = maybe_add_env(c.bcx, c);
        ret {bcx: c.bcx, val: val, kind: kind};
      }
    }
}

fn int_cast(bcx: @block_ctxt, lldsttype: TypeRef, llsrctype: TypeRef,
            llsrc: ValueRef, signed: bool) -> ValueRef {
    let srcsz = llvm::LLVMGetIntTypeWidth(llsrctype);
    let dstsz = llvm::LLVMGetIntTypeWidth(lldsttype);
    ret if dstsz == srcsz {
            BitCast(bcx, llsrc, lldsttype)
        } else if srcsz > dstsz {
            TruncOrBitCast(bcx, llsrc, lldsttype)
        } else if signed {
            SExtOrBitCast(bcx, llsrc, lldsttype)
        } else { ZExtOrBitCast(bcx, llsrc, lldsttype) };
}

fn float_cast(bcx: @block_ctxt, lldsttype: TypeRef, llsrctype: TypeRef,
              llsrc: ValueRef) -> ValueRef {
    let srcsz = lib::llvm::float_width(llsrctype);
    let dstsz = lib::llvm::float_width(lldsttype);
    ret if dstsz > srcsz {
            FPExt(bcx, llsrc, lldsttype)
        } else if srcsz > dstsz {
            FPTrunc(bcx, llsrc, lldsttype)
        } else { llsrc };
}

fn trans_cast(cx: @block_ctxt, e: @ast::expr, id: ast::node_id,
              dest: dest) -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    let t_out = node_id_type(ccx, id);
    alt ty::struct(ccx.tcx, t_out) {
      ty::ty_iface(_, _) { ret trans_impl::trans_cast(cx, e, id, dest); }
      _ {}
    }
    let e_res = trans_temp_expr(cx, e);
    let ll_t_in = val_ty(e_res.val);
    let t_in = ty::expr_ty(ccx.tcx, e);
    // Check should be avoidable because it's a cast.
    // FIXME: Constrain types so as to avoid this check.
    check (type_has_static_size(ccx, t_out));
    let ll_t_out = type_of(ccx, e.span, t_out);

    tag kind { pointer; integral; float; tag_; other; }
    fn t_kind(tcx: ty::ctxt, t: ty::t) -> kind {
        ret if ty::type_is_fp(tcx, t) {
                float
            } else if ty::type_is_native(tcx, t) ||
                      ty::type_is_unsafe_ptr(tcx, t) {
                pointer
            } else if ty::type_is_integral(tcx, t) {
                integral
            } else if ty::type_is_tag(tcx, t) {
                tag_
            } else { other };
    }
    let k_in = t_kind(ccx.tcx, t_in);
    let k_out = t_kind(ccx.tcx, t_out);
    let s_in = k_in == integral && ty::type_is_signed(ccx.tcx, t_in);

    let newval =
        alt {in: k_in, out: k_out} {
          {in: integral., out: integral.} {
            int_cast(e_res.bcx, ll_t_out, ll_t_in, e_res.val, s_in)
          }
          {in: float., out: float.} {
            float_cast(e_res.bcx, ll_t_out, ll_t_in, e_res.val)
          }
          {in: integral., out: float.} {
            if s_in {
                SIToFP(e_res.bcx, e_res.val, ll_t_out)
            } else { UIToFP(e_res.bcx, e_res.val, ll_t_out) }
          }
          {in: float., out: integral.} {
            if ty::type_is_signed(ccx.tcx, t_out) {
                FPToSI(e_res.bcx, e_res.val, ll_t_out)
            } else { FPToUI(e_res.bcx, e_res.val, ll_t_out) }
          }
          {in: integral., out: pointer.} {
            IntToPtr(e_res.bcx, e_res.val, ll_t_out)
          }
          {in: pointer., out: integral.} {
            PtrToInt(e_res.bcx, e_res.val, ll_t_out)
          }
          {in: pointer., out: pointer.} {
            PointerCast(e_res.bcx, e_res.val, ll_t_out)
          }
          {in: tag_., out: integral.} | {in: tag_., out: float.} {
            let cx = e_res.bcx;
            let lltagty = T_opaque_tag_ptr(ccx);
            let av_tag = PointerCast(cx, e_res.val, lltagty);
            let lldiscrim_a_ptr = GEPi(cx, av_tag, [0, 0]);
            let lldiscrim_a = Load(cx, lldiscrim_a_ptr);
            alt k_out {
              integral. {int_cast(e_res.bcx, ll_t_out,
                                  val_ty(lldiscrim_a), lldiscrim_a, true)}
              float. {SIToFP(e_res.bcx, lldiscrim_a, ll_t_out)}
            }
          }
          _ { ccx.sess.bug("Translating unsupported cast.") }
        };
    ret store_in_dest(e_res.bcx, newval, dest);
}

fn trans_arg_expr(cx: @block_ctxt, arg: ty::arg, lldestty: TypeRef,
                  &to_zero: [{v: ValueRef, t: ty::t}],
                  &to_revoke: [{v: ValueRef, t: ty::t}], e: @ast::expr) ->
   result {
    let ccx = bcx_ccx(cx);
    let e_ty = ty::expr_ty(ccx.tcx, e);
    let is_bot = ty::type_is_bot(ccx.tcx, e_ty);
    let lv = trans_temp_lval(cx, e);
    let bcx = lv.bcx;
    let val = lv.val;
    if is_bot {
        // For values of type _|_, we generate an
        // "undef" value, as such a value should never
        // be inspected. It's important for the value
        // to have type lldestty (the callee's expected type).
        val = llvm::LLVMGetUndef(lldestty);
    } else if arg.mode == ast::by_ref || arg.mode == ast::by_val {
        let copied = false, imm = ty::type_is_immediate(ccx.tcx, e_ty);
        if arg.mode == ast::by_ref && lv.kind != owned && imm {
            val = do_spill_noroot(bcx, val);
            copied = true;
        }
        if ccx.copy_map.contains_key(e.id) && lv.kind != temporary {
            if !copied {
                let alloc = alloc_ty(bcx, e_ty);
                bcx = copy_val(alloc.bcx, INIT, alloc.val,
                               load_if_immediate(alloc.bcx, val, e_ty), e_ty);
                val = alloc.val;
            } else { bcx = take_ty(bcx, val, e_ty); }
            add_clean(bcx, val, e_ty);
        }
        if arg.mode == ast::by_val && (lv.kind == owned || !imm) {
            val = Load(bcx, val);
        }
    } else if arg.mode == ast::by_copy {
        let {bcx: cx, val: alloc} = alloc_ty(bcx, e_ty);
        let last_use = ccx.last_uses.contains_key(e.id);
        bcx = cx;
        if lv.kind == temporary { revoke_clean(bcx, val); }
        if lv.kind == owned || !ty::type_is_immediate(ccx.tcx, e_ty) {
            bcx = memmove_ty(bcx, alloc, val, e_ty);
            if last_use && ty::type_needs_drop(ccx.tcx, e_ty) {
                bcx = zero_alloca(bcx, val, e_ty);
            }
        } else { Store(bcx, val, alloc); }
        val = alloc;
        if lv.kind != temporary && !last_use {
            bcx = take_ty(bcx, val, e_ty);
        }
    } else if ty::type_is_immediate(ccx.tcx, e_ty) && lv.kind != owned {
        let r = do_spill(bcx, val, e_ty);
        val = r.val;
        bcx = r.bcx;
    }

    if !is_bot && ty::type_contains_params(ccx.tcx, arg.ty) {
        val = PointerCast(bcx, val, lldestty);
    }

    // Collect arg for later if it happens to be one we've moving out.
    if arg.mode == ast::by_move {
        if lv.kind == owned {
            // Use actual ty, not declared ty -- anything else doesn't make
            // sense if declared ty is a ty param
            to_zero += [{v: lv.val, t: e_ty}];
        } else { to_revoke += [{v: lv.val, t: e_ty}]; }
    }
    ret rslt(bcx, val);
}


// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_llargs_for_fn_args.
//  - new_fn_ctxt
//  - trans_args
fn trans_args(cx: @block_ctxt, llenv: ValueRef,
              gen: option::t<generic_info>, es: [@ast::expr], fn_ty: ty::t,
              dest: dest)
   -> {bcx: @block_ctxt,
       args: [ValueRef],
       retslot: ValueRef,
       to_zero: [{v: ValueRef, t: ty::t}],
       to_revoke: [{v: ValueRef, t: ty::t}]} {

    let args: [ty::arg] = ty::ty_fn_args(bcx_tcx(cx), fn_ty);
    let llargs: [ValueRef] = [];
    let lltydescs: [ValueRef] = [];
    let to_zero = [];
    let to_revoke = [];

    let ccx = bcx_ccx(cx);
    let tcx = ccx.tcx;
    let bcx = cx;

    let retty = ty::ty_fn_ret(tcx, fn_ty), full_retty = retty;
    alt gen {
      some(g) {
        lazily_emit_all_generic_info_tydesc_glues(cx, g);
        let i = 0u, n_orig = 0u;
        for param in *g.param_bounds {
            lltydescs += [g.tydescs[i]];
            for bound in *param {
                alt bound {
                  ty::bound_iface(_) {
                    let res = trans_impl::get_dict(
                        bcx, option::get(g.origins)[n_orig]);
                    lltydescs += [res.val];
                    bcx = res.bcx;
                    n_orig += 1u;
                  }
                  _ {}
                }
            }
            i += 1u;
        }
        args = ty::ty_fn_args(tcx, g.item_type);
        retty = ty::ty_fn_ret(tcx, g.item_type);
      }
      _ { }
    }
    // Arg 0: Output pointer.
    let llretslot = alt dest {
      ignore. {
        if ty::type_is_nil(tcx, retty) {
            llvm::LLVMGetUndef(T_ptr(T_nil()))
        } else {
            let {bcx: cx, val} = alloc_ty(bcx, full_retty);
            bcx = cx;
            val
        }
      }
      save_in(dst) { dst }
      by_val(_) {
          let {bcx: cx, val} = alloc_ty(bcx, full_retty);
          bcx = cx;
          val
      }
    };

    if ty::type_contains_params(tcx, retty) {
        // It's possible that the callee has some generic-ness somewhere in
        // its return value -- say a method signature within an obj or a fn
        // type deep in a structure -- which the caller has a concrete view
        // of. If so, cast the caller's view of the restlot to the callee's
        // view, for the sake of making a type-compatible call.
        check non_ty_var(ccx, retty);
        let llretty = T_ptr(type_of_inner(ccx, bcx.sp, retty));
        llargs += [PointerCast(cx, llretslot, llretty)];
    } else { llargs += [llretslot]; }

    // Arg 1: Env (closure-bindings / self value)
    llargs += [llenv];

    // Args >2: ty_params ...
    llargs += lltydescs;

    // ... then explicit args.

    // First we figure out the caller's view of the types of the arguments.
    // This will be needed if this is a generic call, because the callee has
    // to cast her view of the arguments to the caller's view.
    let arg_tys = type_of_explicit_args(ccx, cx.sp, args);
    let i = 0u;
    for e: @ast::expr in es {
        let r = trans_arg_expr(bcx, args[i], arg_tys[i], to_zero, to_revoke,
                               e);
        bcx = r.bcx;
        llargs += [r.val];
        i += 1u;
    }
    ret {bcx: bcx,
         args: llargs,
         retslot: llretslot,
         to_zero: to_zero,
         to_revoke: to_revoke};
}

fn trans_call(in_cx: @block_ctxt, f: @ast::expr,
              args: [@ast::expr], id: ast::node_id, dest: dest)
    -> @block_ctxt {
    // NB: 'f' isn't necessarily a function; it might be an entire self-call
    // expression because of the hack that allows us to process self-calls
    // with trans_call.
    let tcx = bcx_tcx(in_cx);
    let fn_expr_ty = ty::expr_ty(tcx, f);

    let cx = new_scope_block_ctxt(in_cx, "call");
    Br(in_cx, cx.llbb);
    let f_res = trans_callee(cx, f);
    let bcx = f_res.bcx;

    let faddr = f_res.val;
    let llenv, dict_param = none;
    alt f_res.env {
      null_env. {
        llenv = llvm::LLVMGetUndef(T_opaque_cbox_ptr(bcx_ccx(cx)));
      }
      self_env(e) { llenv = e; }
      dict_env(dict, e) { llenv = e; dict_param = some(dict); }
      is_closure. {
        // It's a closure. Have to fetch the elements
        if f_res.kind == owned {
            faddr = load_if_immediate(bcx, faddr, fn_expr_ty);
        }
        let pair = faddr;
        faddr = GEPi(bcx, pair, [0, abi::fn_field_code]);
        faddr = Load(bcx, faddr);
        let llclosure = GEPi(bcx, pair, [0, abi::fn_field_box]);
        llenv = Load(bcx, llclosure);
      }
    }

    let ret_ty = ty::node_id_to_type(tcx, id);
    let args_res =
        trans_args(bcx, llenv, f_res.generic, args, fn_expr_ty, dest);
    bcx = args_res.bcx;
    let llargs = args_res.args;
    option::may(dict_param) {|dict| llargs = [dict] + llargs}
    let llretslot = args_res.retslot;

    /* If the block is terminated,
       then one or more of the args has
       type _|_. Since that means it diverges, the code
       for the call itself is unreachable. */
    bcx = invoke_full(bcx, faddr, llargs, args_res.to_zero,
                      args_res.to_revoke);
    alt dest {
      ignore. {
        if llvm::LLVMIsUndef(llretslot) != lib::llvm::True {
            bcx = drop_ty(bcx, llretslot, ret_ty);
        }
      }
      save_in(_) { } // Already saved by callee
      by_val(cell) {
        *cell = Load(bcx, llretslot);
      }
    }
    // Forget about anything we moved out.
    bcx = zero_and_revoke(bcx, args_res.to_zero, args_res.to_revoke);

    bcx = trans_block_cleanups(bcx, cx);
    let next_cx = new_sub_block_ctxt(in_cx, "next");
    if bcx.unreachable || ty::type_is_bot(tcx, ret_ty) {
        Unreachable(next_cx);
    }
    Br(bcx, next_cx.llbb);
    ret next_cx;
}

fn zero_and_revoke(bcx: @block_ctxt,
                   to_zero: [{v: ValueRef, t: ty::t}],
                   to_revoke: [{v: ValueRef, t: ty::t}]) -> @block_ctxt {
    let bcx = bcx;
    for {v, t} in to_zero {
        bcx = zero_alloca(bcx, v, t);
    }
    for {v, _} in to_revoke { revoke_clean(bcx, v); }
    ret bcx;
}

fn invoke(bcx: @block_ctxt, llfn: ValueRef,
          llargs: [ValueRef]) -> @block_ctxt {
    ret invoke_(bcx, llfn, llargs, [], [], Invoke);
}

fn invoke_full(bcx: @block_ctxt, llfn: ValueRef, llargs: [ValueRef],
               to_zero: [{v: ValueRef, t: ty::t}],
               to_revoke: [{v: ValueRef, t: ty::t}]) -> @block_ctxt {
    ret invoke_(bcx, llfn, llargs, to_zero, to_revoke, Invoke);
}

fn invoke_(bcx: @block_ctxt, llfn: ValueRef, llargs: [ValueRef],
           to_zero: [{v: ValueRef, t: ty::t}],
           to_revoke: [{v: ValueRef, t: ty::t}],
           invoker: block(@block_ctxt, ValueRef, [ValueRef],
                          BasicBlockRef, BasicBlockRef)) -> @block_ctxt {
    // FIXME: May be worth turning this into a plain call when there are no
    // cleanups to run
    if bcx.unreachable { ret bcx; }
    let normal_bcx = new_sub_block_ctxt(bcx, "normal return");
    invoker(bcx, llfn, llargs, normal_bcx.llbb,
            get_landing_pad(bcx, to_zero, to_revoke));
    ret normal_bcx;
}

fn get_landing_pad(bcx: @block_ctxt,
                   to_zero: [{v: ValueRef, t: ty::t}],
                   to_revoke: [{v: ValueRef, t: ty::t}]
                  ) -> BasicBlockRef {
    let have_zero_or_revoke = vec::is_not_empty(to_zero)
        || vec::is_not_empty(to_revoke);
    let scope_bcx = find_scope_for_lpad(bcx, have_zero_or_revoke);
    if scope_bcx.lpad_dirty || have_zero_or_revoke {
        let unwind_bcx = new_sub_block_ctxt(bcx, "unwind");
        trans_landing_pad(unwind_bcx, to_zero, to_revoke);
        scope_bcx.lpad = some(unwind_bcx.llbb);
        scope_bcx.lpad_dirty = have_zero_or_revoke;
    }
    assert option::is_some(scope_bcx.lpad);
    ret option::get(scope_bcx.lpad);

    fn find_scope_for_lpad(bcx: @block_ctxt,
                           have_zero_or_revoke: bool) -> @block_ctxt {
        let scope_bcx = bcx;
        while true {
            scope_bcx = find_scope_cx(scope_bcx);
            if vec::is_not_empty(scope_bcx.cleanups)
                || have_zero_or_revoke {
                ret scope_bcx;
            } else {
                scope_bcx = alt scope_bcx.parent {
                  parent_some(b) { b }
                  parent_none. {
                    ret scope_bcx;
                  }
                };
            }
        }
        fail;
    }
}

fn trans_landing_pad(bcx: @block_ctxt,
                     to_zero: [{v: ValueRef, t: ty::t}],
                     to_revoke: [{v: ValueRef, t: ty::t}]) -> BasicBlockRef {
    // The landing pad return type (the type being propagated). Not sure what
    // this represents but it's determined by the personality function and
    // this is what the EH proposal example uses.
    let llretty = T_struct([T_ptr(T_i8()), T_i32()]);
    // The exception handling personality function. This is the C++
    // personality function __gxx_personality_v0, wrapped in our naming
    // convention.
    let personality = bcx_ccx(bcx).upcalls.rust_personality;
    // The only landing pad clause will be 'cleanup'
    let clauses = 1u;
    let llpad = LandingPad(bcx, llretty, personality, clauses);
    // The landing pad result is used both for modifying the landing pad
    // in the C API and as the exception value
    let llretval = llpad;
    // The landing pad block is a cleanup
    SetCleanup(bcx, llpad);

    // Because we may have unwound across a stack boundary, we must call into
    // the runtime to figure out which stack segment we are on and place the
    // stack limit back into the TLS.
    Call(bcx, bcx_ccx(bcx).upcalls.reset_stack_limit, []);

    // FIXME: This seems like a very naive and redundant way to generate the
    // landing pads, as we're re-generating all in-scope cleanups for each
    // function call. Probably good optimization opportunities here.
    let bcx = zero_and_revoke(bcx, to_zero, to_revoke);
    let scope_cx = bcx;
    while true {
        scope_cx = find_scope_cx(scope_cx);
        bcx = trans_block_cleanups(bcx, scope_cx);
        scope_cx = alt scope_cx.parent {
          parent_some(b) { b }
          parent_none. { break; }
        };
    }

    // Continue unwinding
    Resume(bcx, llretval);
    ret bcx.llbb;
}

fn trans_tup(bcx: @block_ctxt, elts: [@ast::expr], id: ast::node_id,
             dest: dest) -> @block_ctxt {
    let t = node_id_type(bcx.fcx.lcx.ccx, id);
    let bcx = bcx;
    let addr = alt dest {
      ignore. {
        for ex in elts { bcx = trans_expr(bcx, ex, ignore); }
        ret bcx;
      }
      save_in(pos) { pos }
    };
    let temp_cleanups = [], i = 0;
    for e in elts {
        let dst = GEP_tup_like_1(bcx, t, addr, [0, i]);
        let e_ty = ty::expr_ty(bcx_tcx(bcx), e);
        bcx = trans_expr_save_in(dst.bcx, e, dst.val);
        add_clean_temp_mem(bcx, dst.val, e_ty);
        temp_cleanups += [dst.val];
        i += 1;
    }
    for cleanup in temp_cleanups { revoke_clean(bcx, cleanup); }
    ret bcx;
}

fn trans_rec(bcx: @block_ctxt, fields: [ast::field],
             base: option::t<@ast::expr>, id: ast::node_id,
             dest: dest) -> @block_ctxt {
    let t = node_id_type(bcx_ccx(bcx), id);
    let bcx = bcx;
    let addr = alt dest {
      ignore. {
        for fld in fields {
            bcx = trans_expr(bcx, fld.node.expr, ignore);
        }
        ret bcx;
      }
      save_in(pos) { pos }
    };

    let ty_fields = alt ty::struct(bcx_tcx(bcx), t) { ty::ty_rec(f) { f } };
    let temp_cleanups = [];
    for fld in fields {
        let ix = option::get(vec::position_pred(ty_fields, {|ft|
            str::eq(fld.node.ident, ft.ident)
        }));
        let dst = GEP_tup_like_1(bcx, t, addr, [0, ix as int]);
        bcx = trans_expr_save_in(dst.bcx, fld.node.expr, dst.val);
        add_clean_temp_mem(bcx, dst.val, ty_fields[ix].mt.ty);
        temp_cleanups += [dst.val];
    }
    alt base {
      some(bexp) {
        let {bcx: cx, val: base_val} = trans_temp_expr(bcx, bexp), i = 0;
        bcx = cx;
        // Copy over inherited fields
        for tf in ty_fields {
            if !vec::any(fields, {|f| str::eq(f.node.ident, tf.ident)}) {
                let dst = GEP_tup_like_1(bcx, t, addr, [0, i]);
                let base = GEP_tup_like_1(bcx, t, base_val, [0, i]);
                let val = load_if_immediate(base.bcx, base.val, tf.mt.ty);
                bcx = copy_val(base.bcx, INIT, dst.val, val, tf.mt.ty);
            }
            i += 1;
        }
      }
      none. {}
    };

    // Now revoke the cleanups as we pass responsibility for the data
    // structure on to the caller
    for cleanup in temp_cleanups { revoke_clean(bcx, cleanup); }
    ret bcx;
}

// Store the result of an expression in the given memory location, ensuring
// that nil or bot expressions get ignore rather than save_in as destination.
fn trans_expr_save_in(bcx: @block_ctxt, e: @ast::expr, dest: ValueRef)
    -> @block_ctxt {
    let tcx = bcx_tcx(bcx), t = ty::expr_ty(tcx, e);
    let do_ignore = ty::type_is_bot(tcx, t) || ty::type_is_nil(tcx, t);
    ret trans_expr(bcx, e, do_ignore ? ignore : save_in(dest));
}

// Call this to compile an expression that you need as an intermediate value,
// and you want to know whether you're dealing with an lval or not (the kind
// field in the returned struct). For non-intermediates, use trans_expr or
// trans_expr_save_in. For intermediates where you don't care about lval-ness,
// use trans_temp_expr.
fn trans_temp_lval(bcx: @block_ctxt, e: @ast::expr) -> lval_result {
    let bcx = bcx;
    if expr_is_lval(bcx, e) {
        ret trans_lval(bcx, e);
    } else {
        let tcx = bcx_tcx(bcx);
        let ty = ty::expr_ty(tcx, e);
        if ty::type_is_nil(tcx, ty) || ty::type_is_bot(tcx, ty) {
            bcx = trans_expr(bcx, e, ignore);
            ret {bcx: bcx, val: C_nil(), kind: temporary};
        } else if ty::type_is_immediate(bcx_tcx(bcx), ty) {
            let cell = empty_dest_cell();
            bcx = trans_expr(bcx, e, by_val(cell));
            add_clean_temp(bcx, *cell, ty);
            ret {bcx: bcx, val: *cell, kind: temporary};
        } else {
            let {bcx, val: scratch} = alloc_ty(bcx, ty);
            bcx = trans_expr_save_in(bcx, e, scratch);
            add_clean_temp(bcx, scratch, ty);
            ret {bcx: bcx, val: scratch, kind: temporary};
        }
    }
}

// Use only for intermediate values. See trans_expr and trans_expr_save_in for
// expressions that must 'end up somewhere' (or get ignored).
fn trans_temp_expr(bcx: @block_ctxt, e: @ast::expr) -> result {
    let {bcx, val, kind} = trans_temp_lval(bcx, e);
    if kind == owned {
        val = load_if_immediate(bcx, val, ty::expr_ty(bcx_tcx(bcx), e));
    }
    ret {bcx: bcx, val: val};
}

// Translate an expression, with the dest argument deciding what happens with
// the result. Invariants:
// - exprs returning nil or bot always get dest=ignore
// - exprs with non-immediate type never get dest=by_val
fn trans_expr(bcx: @block_ctxt, e: @ast::expr, dest: dest) -> @block_ctxt {
    let tcx = bcx_tcx(bcx);
    debuginfo::update_source_pos(bcx, e.span);

    if expr_is_lval(bcx, e) {
        ret lval_to_dps(bcx, e, dest);
    }

    alt e.node {
      ast::expr_if(cond, thn, els) | ast::expr_if_check(cond, thn, els) {
        ret trans_if(bcx, cond, thn, els, dest);
      }
      ast::expr_ternary(_, _, _) {
        ret trans_expr(bcx, ast_util::ternary_to_if(e), dest);
      }
      ast::expr_alt(expr, arms) {
          //          tcx.sess.span_note(e.span, "about to call trans_alt");
        ret trans_alt::trans_alt(bcx, expr, arms, dest);
      }
      ast::expr_block(blk) {
        let sub_cx = new_scope_block_ctxt(bcx, "block-expr body");
        Br(bcx, sub_cx.llbb);
        sub_cx = trans_block_dps(sub_cx, blk, dest);
        let next_cx = new_sub_block_ctxt(bcx, "next");
        Br(sub_cx, next_cx.llbb);
        if sub_cx.unreachable { Unreachable(next_cx); }
        ret next_cx;
      }
      ast::expr_rec(args, base) {
        ret trans_rec(bcx, args, base, e.id, dest);
      }
      ast::expr_tup(args) { ret trans_tup(bcx, args, e.id, dest); }
      ast::expr_lit(lit) { ret trans_lit(bcx, *lit, dest); }
      ast::expr_vec(args, _) { ret tvec::trans_vec(bcx, args, e.id, dest); }
      ast::expr_binary(op, x, y) { ret trans_binary(bcx, op, x, y, dest); }
      ast::expr_unary(op, x) {
        assert op != ast::deref; // lvals are handled above
        ret trans_unary(bcx, op, x, e.id, dest);
      }
      ast::expr_fn(proto, decl, body, cap_clause) {
        ret trans_closure::trans_expr_fn(
            bcx, proto, decl, body, e.span, e.id, *cap_clause, dest);
      }
      ast::expr_fn_block(decl, body) {
        alt ty::struct(tcx, ty::expr_ty(tcx, e)) {
          ty::ty_fn({proto, _}) {
            #debug("translating fn_block %s with type %s",
                   expr_to_str(e), ty_to_str(tcx, ty::expr_ty(tcx, e)));
            let cap_clause = { copies: [], moves: [] };
            ret trans_closure::trans_expr_fn(
                bcx, proto, decl, body, e.span, e.id, cap_clause, dest);
          }
          _ {
            fail "Type of fn block is not a function!";
          }
        }
      }
      ast::expr_bind(f, args) {
        ret trans_closure::trans_bind(
            bcx, f, args, e.id, dest);
      }
      ast::expr_copy(a) {
        if !expr_is_lval(bcx, a) {
            ret trans_expr(bcx, a, dest);
        }
        else { ret lval_to_dps(bcx, a, dest); }
      }
      ast::expr_cast(val, _) { ret trans_cast(bcx, val, e.id, dest); }
      ast::expr_call(f, args, _) {
        ret trans_call(bcx, f, args, e.id, dest);
      }
      ast::expr_field(_, _, _) {
        fail "Taking the value of a method does not work yet (issue #435)";
      }

      // These return nothing
      ast::expr_break. {
        assert dest == ignore;
        ret trans_break(e.span, bcx);
      }
      ast::expr_cont. {
        assert dest == ignore;
        ret trans_cont(e.span, bcx);
      }
      ast::expr_ret(ex) {
        assert dest == ignore;
        ret trans_ret(bcx, ex);
      }
      ast::expr_be(ex) {
        // Ideally, the expr_be tag would have a precondition
        // that is_call_expr(ex) -- but we don't support that
        // yet
        // FIXME
        check (ast_util::is_call_expr(ex));
        ret trans_be(bcx, ex);
      }
      ast::expr_fail(expr) {
        assert dest == ignore;
        ret trans_fail_expr(bcx, some(e.span), expr);
      }
      ast::expr_log(_, lvl, a) {
        assert dest == ignore;
        ret trans_log(lvl, bcx, a);
      }
      ast::expr_assert(a) {
        assert dest == ignore;
        ret trans_check_expr(bcx, a, "Assertion");
      }
      ast::expr_check(ast::checked_expr., a) {
        assert dest == ignore;
        ret trans_check_expr(bcx, a, "Predicate");
      }
      ast::expr_check(ast::claimed_expr., a) {
        assert dest == ignore;
        /* Claims are turned on and off by a global variable
           that the RTS sets. This case generates code to
           check the value of that variable, doing nothing
           if it's set to false and acting like a check
           otherwise. */
        let c =
            get_extern_const(bcx_ccx(bcx).externs, bcx_ccx(bcx).llmod,
                             "check_claims", T_bool());
        let cond = Load(bcx, c);

        let then_cx = new_scope_block_ctxt(bcx, "claim_then");
        let check_cx = trans_check_expr(then_cx, a, "Claim");
        let next_cx = new_sub_block_ctxt(bcx, "join");

        CondBr(bcx, cond, then_cx.llbb, next_cx.llbb);
        Br(check_cx, next_cx.llbb);
        ret next_cx;
      }
      ast::expr_for(decl, seq, body) {
        assert dest == ignore;
        ret trans_for(bcx, decl, seq, body);
      }
      ast::expr_while(cond, body) {
        assert dest == ignore;
        ret trans_while(bcx, cond, body);
      }
      ast::expr_do_while(body, cond) {
        assert dest == ignore;
        ret trans_do_while(bcx, body, cond);
      }
      ast::expr_assign(dst, src) {
        assert dest == ignore;
        let src_r = trans_temp_lval(bcx, src);
        let {bcx, val: addr, kind} = trans_lval(src_r.bcx, dst);
        assert kind == owned;
        ret store_temp_expr(bcx, DROP_EXISTING, addr, src_r,
                            ty::expr_ty(bcx_tcx(bcx), src),
                            bcx_ccx(bcx).last_uses.contains_key(src.id));
      }
      ast::expr_move(dst, src) {
        // FIXME: calculate copy init-ness in typestate.
        assert dest == ignore;
        let src_r = trans_temp_lval(bcx, src);
        let {bcx, val: addr, kind} = trans_lval(src_r.bcx, dst);
        assert kind == owned;
        ret move_val(bcx, DROP_EXISTING, addr, src_r,
                     ty::expr_ty(bcx_tcx(bcx), src));
      }
      ast::expr_swap(dst, src) {
        assert dest == ignore;
        let lhs_res = trans_lval(bcx, dst);
        assert lhs_res.kind == owned;
        let rhs_res = trans_lval(lhs_res.bcx, src);
        let t = ty::expr_ty(tcx, src);
        let {bcx: bcx, val: tmp_alloc} = alloc_ty(rhs_res.bcx, t);
        // Swap through a temporary.
        bcx = move_val(bcx, INIT, tmp_alloc, lhs_res, t);
        bcx = move_val(bcx, INIT, lhs_res.val, rhs_res, t);
        ret move_val(bcx, INIT, rhs_res.val, lval_owned(bcx, tmp_alloc), t);
      }
      ast::expr_assign_op(op, dst, src) {
        assert dest == ignore;
        ret trans_assign_op(bcx, op, dst, src);
      }
    }
}

fn lval_to_dps(bcx: @block_ctxt, e: @ast::expr, dest: dest) -> @block_ctxt {
    let lv = trans_lval(bcx, e), ccx = bcx_ccx(bcx);
    let {bcx, val, kind} = lv;
    let last_use = kind == owned && ccx.last_uses.contains_key(e.id);
    let ty = ty::expr_ty(ccx.tcx, e);
    alt dest {
      by_val(cell) {
        if kind == temporary {
            revoke_clean(bcx, val);
            *cell = val;
        } else if last_use {
            *cell = Load(bcx, val);
            if ty::type_needs_drop(ccx.tcx, ty) {
                bcx = zero_alloca(bcx, val, ty);
            }
        } else {
            if kind == owned { val = Load(bcx, val); }
            let {bcx: cx, val} = take_ty_immediate(bcx, val, ty);
            *cell = val;
            bcx = cx;
        }
      }
      save_in(loc) {
        bcx = store_temp_expr(bcx, INIT, loc, lv, ty, last_use);
      }
      ignore. {}
    }
    ret bcx;
}

fn do_spill(cx: @block_ctxt, v: ValueRef, t: ty::t) -> result {
    // We have a value but we have to spill it, and root it, to pass by alias.
    let bcx = cx;

    if ty::type_is_bot(bcx_tcx(bcx), t) {
        ret rslt(bcx, C_null(T_ptr(T_i8())));
    }

    let r = alloc_ty(bcx, t);
    bcx = r.bcx;
    let llptr = r.val;

    Store(bcx, v, llptr);

    ret rslt(bcx, llptr);
}

// Since this function does *not* root, it is the caller's responsibility to
// ensure that the referent is pointed to by a root.
fn do_spill_noroot(cx: @block_ctxt, v: ValueRef) -> ValueRef {
    let llptr = alloca(cx, val_ty(v));
    Store(cx, v, llptr);
    ret llptr;
}

fn spill_if_immediate(cx: @block_ctxt, v: ValueRef, t: ty::t) -> result {
    if ty::type_is_immediate(bcx_tcx(cx), t) { ret do_spill(cx, v, t); }
    ret rslt(cx, v);
}

fn load_if_immediate(cx: @block_ctxt, v: ValueRef, t: ty::t) -> ValueRef {
    if ty::type_is_immediate(bcx_tcx(cx), t) { ret Load(cx, v); }
    ret v;
}

fn trans_log(lvl: @ast::expr, cx: @block_ctxt, e: @ast::expr) -> @block_ctxt {
    let ccx = bcx_ccx(cx);
    let lcx = cx.fcx.lcx;
    let modname = str::connect(lcx.module_path, "::");
    let global = if lcx.ccx.module_data.contains_key(modname) {
        lcx.ccx.module_data.get(modname)
    } else {
        let s = link::mangle_internal_name_by_path_and_seq(
            lcx.ccx, lcx.module_path, "loglevel");
        let global = str::as_buf(s, {|buf|
            llvm::LLVMAddGlobal(lcx.ccx.llmod, T_i32(), buf)
        });
        llvm::LLVMSetGlobalConstant(global, False);
        llvm::LLVMSetInitializer(global, C_null(T_i32()));
        llvm::LLVMSetLinkage(global,
                             lib::llvm::LLVMInternalLinkage as llvm::Linkage);
        lcx.ccx.module_data.insert(modname, global);
        global
    };
    let level_cx = new_scope_block_ctxt(cx, "level");
    let log_cx = new_scope_block_ctxt(cx, "log");
    let after_cx = new_sub_block_ctxt(cx, "after");
    let load = Load(cx, global);

    Br(cx, level_cx.llbb);
    let level_res = trans_temp_expr(level_cx, lvl);
    let test = ICmp(level_res.bcx, lib::llvm::LLVMIntUGE,
                    load, level_res.val);

    CondBr(level_res.bcx, test, log_cx.llbb, after_cx.llbb);
    let sub = trans_temp_expr(log_cx, e);
    let e_ty = ty::expr_ty(bcx_tcx(cx), e);
    let log_bcx = sub.bcx;

    let ti = none::<@tydesc_info>;
    let r = get_tydesc(log_bcx, e_ty, false, ti).result;
    log_bcx = r.bcx;
    let lltydesc = r.val;

    // Call the polymorphic log function.
    r = spill_if_immediate(log_bcx, sub.val, e_ty);
    log_bcx = r.bcx;
    let llvalptr = r.val;
    let llval_i8 = PointerCast(log_bcx, llvalptr, T_ptr(T_i8()));

    Call(log_bcx, ccx.upcalls.log_type,
         [lltydesc, llval_i8, level_res.val]);

    log_bcx = trans_block_cleanups(log_bcx, log_cx);
    Br(log_bcx, after_cx.llbb);
    ret trans_block_cleanups(after_cx, level_cx);
}

fn trans_check_expr(cx: @block_ctxt, e: @ast::expr, s: str) -> @block_ctxt {
    let cond_res = trans_temp_expr(cx, e);
    let expr_str = s + " " + expr_to_str(e) + " failed";
    let fail_cx = new_sub_block_ctxt(cx, "fail");
    trans_fail(fail_cx, some::<span>(e.span), expr_str);
    let next_cx = new_sub_block_ctxt(cx, "next");
    CondBr(cond_res.bcx, cond_res.val, next_cx.llbb, fail_cx.llbb);
    ret next_cx;
}

fn trans_fail_expr(bcx: @block_ctxt, sp_opt: option::t<span>,
                   fail_expr: option::t<@ast::expr>) -> @block_ctxt {
    let bcx = bcx;
    alt fail_expr {
      some(expr) {
        let tcx = bcx_tcx(bcx);
        let expr_res = trans_temp_expr(bcx, expr);
        let e_ty = ty::expr_ty(tcx, expr);
        bcx = expr_res.bcx;

        if ty::type_is_str(tcx, e_ty) {
            let data = tvec::get_dataptr(
                bcx, expr_res.val, type_of_or_i8(
                    bcx, ty::mk_mach_uint(tcx, ast::ty_u8)));
            ret trans_fail_value(bcx, sp_opt, data);
        } else if bcx.unreachable {
            ret bcx;
        } else {
            bcx_ccx(bcx).sess.span_bug(
                expr.span, "fail called with unsupported type " +
                ty_to_str(tcx, e_ty));
        }
      }
      _ { ret trans_fail(bcx, sp_opt, "explicit failure"); }
    }
}

fn trans_fail(bcx: @block_ctxt, sp_opt: option::t<span>, fail_str: str) ->
    @block_ctxt {
    let V_fail_str = C_cstr(bcx_ccx(bcx), fail_str);
    ret trans_fail_value(bcx, sp_opt, V_fail_str);
}

fn trans_fail_value(bcx: @block_ctxt, sp_opt: option::t<span>,
                    V_fail_str: ValueRef) -> @block_ctxt {
    let ccx = bcx_ccx(bcx);
    let V_filename;
    let V_line;
    alt sp_opt {
      some(sp) {
        let sess = bcx_ccx(bcx).sess;
        let loc = codemap::lookup_char_pos(sess.parse_sess.cm, sp.lo);
        V_filename = C_cstr(bcx_ccx(bcx), loc.filename);
        V_line = loc.line as int;
      }
      none. { V_filename = C_cstr(bcx_ccx(bcx), "<runtime>"); V_line = 0; }
    }
    let V_str = PointerCast(bcx, V_fail_str, T_ptr(T_i8()));
    V_filename = PointerCast(bcx, V_filename, T_ptr(T_i8()));
    let args = [V_str, V_filename, C_int(ccx, V_line)];
    let bcx = invoke(bcx, bcx_ccx(bcx).upcalls._fail, args);
    Unreachable(bcx);
    ret bcx;
}

fn trans_break_cont(sp: span, bcx: @block_ctxt, to_end: bool)
    -> @block_ctxt {
    // Locate closest loop block, outputting cleanup as we go.
    let cleanup_cx = bcx, bcx = bcx;
    while true {
        bcx = trans_block_cleanups(bcx, cleanup_cx);
        alt copy cleanup_cx.kind {
          LOOP_SCOPE_BLOCK(_cont, _break) {
            if to_end {
                Br(bcx, _break.llbb);
            } else {
                alt _cont {
                  option::some(_cont) { Br(bcx, _cont.llbb); }
                  _ { Br(bcx, cleanup_cx.llbb); }
                }
            }
            Unreachable(bcx);
            ret bcx;
          }
          _ {
            alt cleanup_cx.parent {
              parent_some(cx) { cleanup_cx = cx; }
              parent_none. {
                bcx_ccx(bcx).sess.span_fatal
                    (sp, if to_end { "Break" } else { "Cont" } +
                     " outside a loop");
              }
            }
          }
        }
    }
    // If we get here without returning, it's a bug
    bcx_ccx(bcx).sess.bug("in trans::trans_break_cont()");
}

fn trans_break(sp: span, cx: @block_ctxt) -> @block_ctxt {
    ret trans_break_cont(sp, cx, true);
}

fn trans_cont(sp: span, cx: @block_ctxt) -> @block_ctxt {
    ret trans_break_cont(sp, cx, false);
}

fn trans_ret(bcx: @block_ctxt, e: option::t<@ast::expr>) -> @block_ctxt {
    let cleanup_cx = bcx, bcx = bcx;
    alt e {
      some(x) { bcx = trans_expr_save_in(bcx, x, bcx.fcx.llretptr); }
      _ {}
    }
    // run all cleanups and back out.

    let more_cleanups: bool = true;
    while more_cleanups {
        bcx = trans_block_cleanups(bcx, cleanup_cx);
        alt cleanup_cx.parent {
          parent_some(b) { cleanup_cx = b; }
          parent_none. { more_cleanups = false; }
        }
    }
    build_return(bcx);
    Unreachable(bcx);
    ret bcx;
}

fn build_return(bcx: @block_ctxt) { Br(bcx, bcx_fcx(bcx).llreturn); }

// fn trans_be(cx: &@block_ctxt, e: &@ast::expr) -> result {
fn trans_be(cx: @block_ctxt, e: @ast::expr) : ast_util::is_call_expr(e) ->
   @block_ctxt {
    // FIXME: Turn this into a real tail call once
    // calling convention issues are settled
    ret trans_ret(cx, some(e));
}

fn init_local(bcx: @block_ctxt, local: @ast::local) -> @block_ctxt {
    let ty = node_id_type(bcx_ccx(bcx), local.node.id);
    let llptr = alt bcx.fcx.lllocals.find(local.node.id) {
      some(local_mem(v)) { v }
      // This is a local that is kept immediate
      none. {
        let initexpr = alt local.node.init { some({expr, _}) { expr } };
        let {bcx, val, kind} = trans_temp_lval(bcx, initexpr);
        if kind != temporary {
            if kind == owned { val = Load(bcx, val); }
            let rs = take_ty_immediate(bcx, val, ty);
            bcx = rs.bcx; val = rs.val;
            add_clean_temp(bcx, val, ty);
        }
        bcx.fcx.lllocals.insert(local.node.pat.id, local_imm(val));
        ret bcx;
      }
    };

    let bcx = bcx;
    alt local.node.init {
      some(init) {
        if init.op == ast::init_assign || !expr_is_lval(bcx, init.expr) {
            bcx = trans_expr_save_in(bcx, init.expr, llptr);
        } else { // This is a move from an lval, must perform an actual move
            let sub = trans_lval(bcx, init.expr);
            bcx = move_val(sub.bcx, INIT, llptr, sub, ty);
        }
      }
      _ { bcx = zero_alloca(bcx, llptr, ty); }
    }
    // Make a note to drop this slot on the way out.
    add_clean(bcx, llptr, ty);
    ret trans_alt::bind_irrefutable_pat(bcx, local.node.pat, llptr, false);
}

fn init_ref_local(bcx: @block_ctxt, local: @ast::local) -> @block_ctxt {
    let init_expr = option::get(local.node.init).expr;
    let {bcx, val, kind} = trans_lval(bcx, init_expr);
    alt kind {
      owned_imm. { val = do_spill_noroot(bcx, val); }
      owned. {}
    }
    ret trans_alt::bind_irrefutable_pat(bcx, local.node.pat, val, false);
}

fn zero_alloca(cx: @block_ctxt, llptr: ValueRef, t: ty::t)
    -> @block_ctxt {
    let bcx = cx;
    let ccx = bcx_ccx(cx);
    if check type_has_static_size(ccx, t) {
        let sp = cx.sp;
        let llty = type_of(ccx, sp, t);
        Store(bcx, C_null(llty), llptr);
    } else {
        let key = alt ccx.sess.targ_cfg.arch {
          session::arch_x86. | session::arch_arm. { "llvm.memset.p0i8.i32" }
          session::arch_x86_64. { "llvm.memset.p0i8.i64" }
        };
        let i = ccx.intrinsics;
        let memset = i.get(key);
        let dst_ptr = PointerCast(cx, llptr, T_ptr(T_i8()));
        let size = size_of(cx, t);
        bcx = size.bcx;
        let align = C_i32(1i32); // cannot use computed value here.
        let volatile = C_bool(false);
        Call(cx, memset, [dst_ptr, C_u8(0u), size.val, align, volatile]);
    }
    ret bcx;
}

fn trans_stmt(cx: @block_ctxt, s: ast::stmt) -> @block_ctxt {
    // FIXME Fill in cx.sp

    if (!bcx_ccx(cx).sess.opts.no_asm_comments) {
        add_span_comment(cx, s.span, stmt_to_str(s));
    }

    let bcx = cx;
    debuginfo::update_source_pos(cx, s.span);

    alt s.node {
      ast::stmt_expr(e, _) | ast::stmt_semi(e, _) {
        bcx = trans_expr(cx, e, ignore);
      }
      ast::stmt_decl(d, _) {
        alt d.node {
          ast::decl_local(locals) {
            for (style, local) in locals {
                if style == ast::let_copy {
                    bcx = init_local(bcx, local);
                } else {
                    bcx = init_ref_local(bcx, local);
                }
                if bcx_ccx(cx).sess.opts.extra_debuginfo {
                    debuginfo::create_local_var(bcx, local);
                }
            }
          }
          ast::decl_item(i) { trans_item(cx.fcx.lcx, *i); }
        }
      }
      _ { bcx_ccx(cx).sess.unimpl("stmt variant"); }
    }

    ret bcx;
}

// You probably don't want to use this one. See the
// next three functions instead.
fn new_block_ctxt(cx: @fn_ctxt, parent: block_parent, kind: block_kind,
                  name: str) -> @block_ctxt {
    let s = "";
    if cx.lcx.ccx.sess.opts.save_temps ||
           cx.lcx.ccx.sess.opts.debuginfo {
        s = cx.lcx.ccx.names(name);
    }
    let llbb: BasicBlockRef =
        str::as_buf(s, {|buf| llvm::LLVMAppendBasicBlock(cx.llfn, buf) });
    let bcx = @{llbb: llbb,
                mutable terminated: false,
                mutable unreachable: false,
                parent: parent,
                kind: kind,
                mutable cleanups: [],
                mutable lpad_dirty: true,
                mutable lpad: option::none,
                sp: cx.sp,
                fcx: cx};
    alt parent {
      parent_some(cx) {
        if cx.unreachable { Unreachable(bcx); }
      }
      _ {}
    }
    ret bcx;
}


// Use this when you're at the top block of a function or the like.
fn new_top_block_ctxt(fcx: @fn_ctxt) -> @block_ctxt {
    ret new_block_ctxt(fcx, parent_none, SCOPE_BLOCK, "function top level");
}


// Use this when you're at a curly-brace or similar lexical scope.
fn new_scope_block_ctxt(bcx: @block_ctxt, n: str) -> @block_ctxt {
    ret new_block_ctxt(bcx.fcx, parent_some(bcx), SCOPE_BLOCK, n);
}

fn new_loop_scope_block_ctxt(bcx: @block_ctxt, _cont: option::t<@block_ctxt>,
                             _break: @block_ctxt, n: str) -> @block_ctxt {
    ret new_block_ctxt(bcx.fcx, parent_some(bcx),
                       LOOP_SCOPE_BLOCK(_cont, _break), n);
}


// Use this when you're making a general CFG BB within a scope.
fn new_sub_block_ctxt(bcx: @block_ctxt, n: str) -> @block_ctxt {
    ret new_block_ctxt(bcx.fcx, parent_some(bcx), NON_SCOPE_BLOCK, n);
}

fn new_raw_block_ctxt(fcx: @fn_ctxt, llbb: BasicBlockRef) -> @block_ctxt {
    ret @{llbb: llbb,
          mutable terminated: false,
          mutable unreachable: false,
          parent: parent_none,
          kind: NON_SCOPE_BLOCK,
          mutable cleanups: [],
          mutable lpad_dirty: true,
          mutable lpad: option::none,
          sp: fcx.sp,
          fcx: fcx};
}


// trans_block_cleanups: Go through all the cleanups attached to this
// block_ctxt and execute them.
//
// When translating a block that introdces new variables during its scope, we
// need to make sure those variables go out of scope when the block ends.  We
// do that by running a 'cleanup' function for each variable.
// trans_block_cleanups runs all the cleanup functions for the block.
fn trans_block_cleanups(bcx: @block_ctxt, cleanup_cx: @block_ctxt) ->
   @block_ctxt {
    if bcx.unreachable { ret bcx; }
    if cleanup_cx.kind == NON_SCOPE_BLOCK {
        assert (vec::len::<cleanup>(cleanup_cx.cleanups) == 0u);
    }
    let i = vec::len::<cleanup>(cleanup_cx.cleanups), bcx = bcx;
    while i > 0u {
        i -= 1u;
        let c = cleanup_cx.cleanups[i];
        alt c {
          clean(cfn) { bcx = cfn(bcx); }
          clean_temp(_, cfn) { bcx = cfn(bcx); }
        }
    }
    ret bcx;
}

fn trans_fn_cleanups(fcx: @fn_ctxt, cx: @block_ctxt) {
    alt fcx.llobstacktoken {
      some(lltoken_) {
        let lltoken = lltoken_; // satisfy alias checker
        Call(cx, fcx_ccx(fcx).upcalls.dynastack_free, [lltoken]);
      }
      none. {/* nothing to do */ }
    }
}

fn block_locals(b: ast::blk, it: block(@ast::local)) {
    for s: @ast::stmt in b.node.stmts {
        alt s.node {
          ast::stmt_decl(d, _) {
            alt d.node {
              ast::decl_local(locals) {
                for (style, local) in locals {
                    if style == ast::let_copy { it(local); }
                }
              }
              _ {/* fall through */ }
            }
          }
          _ {/* fall through */ }
        }
    }
}

fn llstaticallocas_block_ctxt(fcx: @fn_ctxt) -> @block_ctxt {
    ret @{llbb: fcx.llstaticallocas,
          mutable terminated: false,
          mutable unreachable: false,
          parent: parent_none,
          kind: SCOPE_BLOCK,
          mutable cleanups: [],
          mutable lpad_dirty: true,
          mutable lpad: option::none,
          sp: fcx.sp,
          fcx: fcx};
}

fn llderivedtydescs_block_ctxt(fcx: @fn_ctxt) -> @block_ctxt {
    ret @{llbb: fcx.llderivedtydescs,
          mutable terminated: false,
          mutable unreachable: false,
          parent: parent_none,
          kind: SCOPE_BLOCK,
          mutable cleanups: [],
          mutable lpad_dirty: true,
          mutable lpad: option::none,
          sp: fcx.sp,
          fcx: fcx};
}


fn alloc_ty(cx: @block_ctxt, t: ty::t) -> result {
    let bcx = cx;
    let ccx = bcx_ccx(cx);
    let val =
        if check type_has_static_size(ccx, t) {
            let sp = cx.sp;
            alloca(bcx, type_of(ccx, sp, t))
        } else {
            // NB: we have to run this particular 'size_of' in a
            // block_ctxt built on the llderivedtydescs block for the fn,
            // so that the size dominates the array_alloca that
            // comes next.

            let n = size_of(llderivedtydescs_block_ctxt(bcx.fcx), t);
            bcx.fcx.llderivedtydescs = n.bcx.llbb;
            dynastack_alloca(bcx, T_i8(), n.val, t)
        };

    // NB: since we've pushed all size calculations in this
    // function up to the alloca block, we actually return the
    // block passed into us unmodified; it doesn't really
    // have to be passed-and-returned here, but it fits
    // past caller conventions and may well make sense again,
    // so we leave it as-is.

    if bcx_tcx(cx).sess.opts.do_gc {
        bcx = gc::add_gc_root(bcx, val, t);
    }

    ret rslt(cx, val);
}

fn alloc_local(cx: @block_ctxt, local: @ast::local) -> @block_ctxt {
    let t = node_id_type(bcx_ccx(cx), local.node.id);
    let p = normalize_pat(bcx_tcx(cx), local.node.pat);
    let is_simple = alt p.node {
      ast::pat_ident(_, none.) { true } _ { false }
    };
    // Do not allocate space for locals that can be kept immediate.
    let ccx = bcx_ccx(cx);
    if is_simple && !ccx.mut_map.contains_key(local.node.pat.id) &&
       !ccx.last_uses.contains_key(local.node.pat.id) &&
       ty::type_is_immediate(ccx.tcx, t) {
        alt local.node.init {
          some({op: ast::init_assign., _}) { ret cx; }
          _ {}
        }
    }
    let r = alloc_ty(cx, t);
    alt p.node {
      ast::pat_ident(pth, none.) {
        if bcx_ccx(cx).sess.opts.debuginfo {
            let _: () = str::as_buf(path_to_ident(pth), {|buf|
                llvm::LLVMSetValueName(r.val, buf)
            });
        }
      }
      _ { }
    }
    cx.fcx.lllocals.insert(local.node.id, local_mem(r.val));
    ret r.bcx;
}

fn trans_block(bcx: @block_ctxt, b: ast::blk) -> @block_ctxt {
    trans_block_dps(bcx, b, ignore)
}

fn trans_block_dps(bcx: @block_ctxt, b: ast::blk, dest: dest)
    -> @block_ctxt {
    let bcx = bcx;
    block_locals(b) {|local| bcx = alloc_local(bcx, local); };
    for s: @ast::stmt in b.node.stmts {
        debuginfo::update_source_pos(bcx, b.span);
        bcx = trans_stmt(bcx, *s);
    }
    alt b.node.expr {
      some(e) {
        let bt = ty::type_is_bot(bcx_tcx(bcx), ty::expr_ty(bcx_tcx(bcx), e));
        debuginfo::update_source_pos(bcx, e.span);
        bcx = trans_expr(bcx, e, bt ? ignore : dest);
      }
      _ { assert dest == ignore || bcx.unreachable; }
    }
    let rv = trans_block_cleanups(bcx, find_scope_cx(bcx));
    ret rv;
}

fn new_local_ctxt(ccx: @crate_ctxt) -> @local_ctxt {
    let pth: [str] = [];
    ret @{path: pth,
          module_path: [ccx.link_meta.name],
          ccx: ccx};
}


// Creates the standard quartet of basic blocks: static allocas, copy args,
// derived tydescs, and dynamic allocas.
fn mk_standard_basic_blocks(llfn: ValueRef) ->
   {sa: BasicBlockRef,
    ca: BasicBlockRef,
    dt: BasicBlockRef,
    da: BasicBlockRef,
    rt: BasicBlockRef} {
    ret {sa:
             str::as_buf("static_allocas",
                         {|buf| llvm::LLVMAppendBasicBlock(llfn, buf) }),
         ca:
             str::as_buf("load_env",
                         {|buf| llvm::LLVMAppendBasicBlock(llfn, buf) }),
         dt:
             str::as_buf("derived_tydescs",
                         {|buf| llvm::LLVMAppendBasicBlock(llfn, buf) }),
         da:
             str::as_buf("dynamic_allocas",
                         {|buf| llvm::LLVMAppendBasicBlock(llfn, buf) }),
         rt:
             str::as_buf("return",
                         {|buf| llvm::LLVMAppendBasicBlock(llfn, buf) })};
}


// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_llargs_for_fn_args.
//  - new_fn_ctxt
//  - trans_args
fn new_fn_ctxt_w_id(cx: @local_ctxt, sp: span, llfndecl: ValueRef,
                    id: ast::node_id, rstyle: ast::ret_style)
    -> @fn_ctxt {
    let llbbs = mk_standard_basic_blocks(llfndecl);
    ret @{llfn: llfndecl,
          llenv: llvm::LLVMGetParam(llfndecl, 1u as c_uint),
          llretptr: llvm::LLVMGetParam(llfndecl, 0u as c_uint),
          mutable llstaticallocas: llbbs.sa,
          mutable llloadenv: llbbs.ca,
          mutable llderivedtydescs_first: llbbs.dt,
          mutable llderivedtydescs: llbbs.dt,
          mutable lldynamicallocas: llbbs.da,
          mutable llreturn: llbbs.rt,
          mutable llobstacktoken: none::<ValueRef>,
          mutable llself: none::<val_self_pair>,
          llargs: new_int_hash::<local_val>(),
          lllocals: new_int_hash::<local_val>(),
          llupvars: new_int_hash::<ValueRef>(),
          mutable lltyparams: [],
          derived_tydescs: ty::new_ty_hash(),
          id: id,
          ret_style: rstyle,
          sp: sp,
          lcx: cx};
}

fn new_fn_ctxt(cx: @local_ctxt, sp: span, llfndecl: ValueRef) -> @fn_ctxt {
    ret new_fn_ctxt_w_id(cx, sp, llfndecl, -1, ast::return_val);
}

// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_llargs_for_fn_args.
//  - new_fn_ctxt
//  - trans_args

// create_llargs_for_fn_args: Creates a mapping from incoming arguments to
// allocas created for them.
//
// When we translate a function, we need to map its incoming arguments to the
// spaces that have been created for them (by code in the llallocas field of
// the function's fn_ctxt).  create_llargs_for_fn_args populates the llargs
// field of the fn_ctxt with
fn create_llargs_for_fn_args(cx: @fn_ctxt, ty_self: self_arg,
                             args: [ast::arg], ty_params: [ast::ty_param]) {
    // Skip the implicit arguments 0, and 1.  TODO: Pull out 2u and define
    // it as a constant, since we're using it in several places in trans this
    // way.
    let arg_n = 2u;
    alt ty_self {
      impl_self(tt) {
        cx.llself = some({v: cx.llenv, t: tt});
      }
      no_self. {}
    }
    for tp in ty_params {
        let lltydesc = llvm::LLVMGetParam(cx.llfn, arg_n as c_uint);
        let dicts = none;
        arg_n += 1u;
        for bound in *fcx_tcx(cx).ty_param_bounds.get(tp.id) {
            alt bound {
              ty::bound_iface(_) {
                let dict = llvm::LLVMGetParam(cx.llfn, arg_n as c_uint);
                arg_n += 1u;
                dicts = some(alt dicts {
                    none. { [dict] }
                    some(ds) { ds + [dict] }
                });
              }
              _ {}
            }
        }
        cx.lltyparams += [{desc: lltydesc, dicts: dicts}];
    }

    // Populate the llargs field of the function context with the ValueRefs
    // that we get from llvm::LLVMGetParam for each argument.
    for arg: ast::arg in args {
        let llarg = llvm::LLVMGetParam(cx.llfn, arg_n as c_uint);
        assert (llarg as int != 0);
        // Note that this uses local_mem even for things passed by value.
        // copy_args_to_allocas will overwrite the table entry with local_imm
        // before it's actually used.
        cx.llargs.insert(arg.id, local_mem(llarg));
        arg_n += 1u;
    }
}

fn copy_args_to_allocas(fcx: @fn_ctxt, bcx: @block_ctxt, args: [ast::arg],
                        arg_tys: [ty::arg]) -> @block_ctxt {
    let arg_n: uint = 0u, bcx = bcx;
    for arg in arg_tys {
        let id = args[arg_n].id;
        let argval = alt fcx.llargs.get(id) { local_mem(v) { v } };
        alt arg.mode {
          ast::by_mut_ref. { }
          ast::by_move. | ast::by_copy. { add_clean(bcx, argval, arg.ty); }
          ast::by_val. {
            if !ty::type_is_immediate(bcx_tcx(bcx), arg.ty) {
                let {bcx: cx, val: alloc} = alloc_ty(bcx, arg.ty);
                bcx = cx;
                Store(bcx, argval, alloc);
                fcx.llargs.insert(id, local_mem(alloc));
            } else {
                fcx.llargs.insert(id, local_imm(argval));
            }
          }
          ast::by_ref. {}
        }
        if fcx_ccx(fcx).sess.opts.extra_debuginfo {
            debuginfo::create_arg(bcx, args[arg_n]);
        }
        arg_n += 1u;
    }
    ret bcx;
}

fn arg_tys_of_fn(ccx: @crate_ctxt, id: ast::node_id) -> [ty::arg] {
    alt ty::struct(ccx.tcx, ty::node_id_to_type(ccx.tcx, id)) {
      ty::ty_fn({inputs, _}) { inputs }
    }
}

// Ties up the llstaticallocas -> llloadenv -> llderivedtydescs ->
// lldynamicallocas -> lltop edges, and builds the return block.
fn finish_fn(fcx: @fn_ctxt, lltop: BasicBlockRef) {
    Br(new_raw_block_ctxt(fcx, fcx.llstaticallocas), fcx.llloadenv);
    Br(new_raw_block_ctxt(fcx, fcx.llloadenv), fcx.llderivedtydescs_first);
    Br(new_raw_block_ctxt(fcx, fcx.llderivedtydescs), fcx.lldynamicallocas);
    Br(new_raw_block_ctxt(fcx, fcx.lldynamicallocas), lltop);

    let ret_cx = new_raw_block_ctxt(fcx, fcx.llreturn);
    trans_fn_cleanups(fcx, ret_cx);
    RetVoid(ret_cx);
}

tag self_arg { impl_self(ty::t); no_self; }

// trans_closure: Builds an LLVM function out of a source function.
// If the function closes over its environment a closure will be
// returned.
fn trans_closure(cx: @local_ctxt, sp: span, decl: ast::fn_decl,
                 body: ast::blk, llfndecl: ValueRef,
                 ty_self: self_arg, ty_params: [ast::ty_param],
                 id: ast::node_id, maybe_load_env: block(@fn_ctxt)) {
    set_uwtable(llfndecl);

    // Set up arguments to the function.
    let fcx = new_fn_ctxt_w_id(cx, sp, llfndecl, id, decl.cf);
    create_llargs_for_fn_args(fcx, ty_self, decl.inputs, ty_params);

    // Create the first basic block in the function and keep a handle on it to
    //  pass to finish_fn later.
    let bcx = new_top_block_ctxt(fcx);
    let lltop = bcx.llbb;
    let block_ty = node_id_type(cx.ccx, body.node.id);

    let arg_tys = arg_tys_of_fn(fcx.lcx.ccx, id);
    bcx = copy_args_to_allocas(fcx, bcx, decl.inputs, arg_tys);

    maybe_load_env(fcx);

    // This call to trans_block is the place where we bridge between
    // translation calls that don't have a return value (trans_crate,
    // trans_mod, trans_item, et cetera) and those that do
    // (trans_block, trans_expr, et cetera).
    if option::is_none(body.node.expr) ||
       ty::type_is_bot(cx.ccx.tcx, block_ty) ||
       ty::type_is_nil(cx.ccx.tcx, block_ty) {
        bcx = trans_block(bcx, body);
    } else {
        bcx = trans_block_dps(bcx, body, save_in(fcx.llretptr));
    }

    // FIXME: until LLVM has a unit type, we are moving around
    // C_nil values rather than their void type.
    if !bcx.unreachable { build_return(bcx); }
    // Insert the mandatory first few basic blocks before lltop.
    finish_fn(fcx, lltop);
}

// trans_fn: creates an LLVM function corresponding to a source language
// function.
fn trans_fn(cx: @local_ctxt, sp: span, decl: ast::fn_decl, body: ast::blk,
            llfndecl: ValueRef, ty_self: self_arg, ty_params: [ast::ty_param],
            id: ast::node_id) {
    let do_time = cx.ccx.sess.opts.stats;
    let start = do_time ? time::get_time() : {sec: 0u32, usec: 0u32};
    let fcx = option::none;
    trans_closure(cx, sp, decl, body, llfndecl, ty_self, ty_params, id,
                  {|new_fcx| fcx = option::some(new_fcx);});
    if cx.ccx.sess.opts.extra_debuginfo {
        debuginfo::create_function(option::get(fcx));
    }
    if do_time {
        let end = time::get_time();
        log_fn_time(cx.ccx, str::connect(cx.path, "::"), start, end);
    }
}

fn trans_res_ctor(cx: @local_ctxt, sp: span, dtor: ast::fn_decl,
                  ctor_id: ast::node_id, ty_params: [ast::ty_param]) {
    let ccx = cx.ccx;

    // Create a function for the constructor
    let llctor_decl;
    alt ccx.item_ids.find(ctor_id) {
      some(x) { llctor_decl = x; }
      _ { ccx.sess.span_fatal(sp, "unbound ctor_id in trans_res_ctor"); }
    }
    let fcx = new_fn_ctxt(cx, sp, llctor_decl);
    let ret_t = ty::ret_ty_of_fn(cx.ccx.tcx, ctor_id);
    create_llargs_for_fn_args(fcx, no_self, dtor.inputs, ty_params);
    let bcx = new_top_block_ctxt(fcx);
    let lltop = bcx.llbb;
    let arg_t = arg_tys_of_fn(ccx, ctor_id)[0].ty;
    let tup_t = ty::mk_tup(ccx.tcx, [ty::mk_int(ccx.tcx), arg_t]);
    let arg = alt fcx.llargs.find(dtor.inputs[0].id) {
      some(local_mem(x)) { x }
    };
    let llretptr = fcx.llretptr;
    if ty::type_has_dynamic_size(ccx.tcx, ret_t) {
        let llret_t = T_ptr(T_struct([ccx.int_type, llvm::LLVMTypeOf(arg)]));
        llretptr = BitCast(bcx, llretptr, llret_t);
    }

    // FIXME: silly checks
    check type_is_tup_like(bcx, tup_t);
    let {bcx, val: dst} = GEP_tup_like(bcx, tup_t, llretptr, [0, 1]);
    bcx = memmove_ty(bcx, dst, arg, arg_t);
    check type_is_tup_like(bcx, tup_t);
    let flag = GEP_tup_like(bcx, tup_t, llretptr, [0, 0]);
    bcx = flag.bcx;
    // FIXME #1184: Resource flag is larger than necessary
    let one = C_int(ccx, 1);
    Store(bcx, one, flag.val);
    build_return(bcx);
    finish_fn(fcx, lltop);
}


fn trans_tag_variant(cx: @local_ctxt, tag_id: ast::node_id,
                     variant: ast::variant, disr: int, is_degen: bool,
                     ty_params: [ast::ty_param]) {
    let ccx = cx.ccx;

    if vec::len::<ast::variant_arg>(variant.node.args) == 0u {
        ret; // nullary constructors are just constants

    }
    // Translate variant arguments to function arguments.

    let fn_args: [ast::arg] = [];
    let i = 0u;
    for varg: ast::variant_arg in variant.node.args {
        fn_args +=
            [{mode: ast::by_copy,
              ty: varg.ty,
              ident: "arg" + uint::to_str(i, 10u),
              id: varg.id}];
    }
    assert (ccx.item_ids.contains_key(variant.node.id));
    let llfndecl: ValueRef;
    alt ccx.item_ids.find(variant.node.id) {
      some(x) { llfndecl = x; }
      _ {
        ccx.sess.span_fatal(variant.span,
                               "unbound variant id in trans_tag_variant");
      }
    }
    let fcx = new_fn_ctxt(cx, variant.span, llfndecl);
    create_llargs_for_fn_args(fcx, no_self, fn_args, ty_params);
    let ty_param_substs: [ty::t] = [];
    i = 0u;
    for tp: ast::ty_param in ty_params {
        ty_param_substs += [ty::mk_param(ccx.tcx, i,
                                         ast_util::local_def(tp.id))];
        i += 1u;
    }
    let arg_tys = arg_tys_of_fn(ccx, variant.node.id);
    let bcx = new_top_block_ctxt(fcx);
    let lltop = bcx.llbb;
    bcx = copy_args_to_allocas(fcx, bcx, fn_args, arg_tys);

    // Cast the tag to a type we can GEP into.
    let llblobptr =
        if is_degen {
            fcx.llretptr
        } else {
            let lltagptr =
                PointerCast(bcx, fcx.llretptr, T_opaque_tag_ptr(ccx));
            let lldiscrimptr = GEPi(bcx, lltagptr, [0, 0]);
            Store(bcx, C_int(ccx, disr), lldiscrimptr);
            GEPi(bcx, lltagptr, [0, 1])
        };
    i = 0u;
    let t_id = ast_util::local_def(tag_id);
    let v_id = ast_util::local_def(variant.node.id);
    for va: ast::variant_arg in variant.node.args {
        check (valid_variant_index(i, bcx, t_id, v_id));
        let rslt = GEP_tag(bcx, llblobptr, t_id, v_id, ty_param_substs, i);
        bcx = rslt.bcx;
        let lldestptr = rslt.val;
        // If this argument to this function is a tag, it'll have come in to
        // this function as an opaque blob due to the way that type_of()
        // works. So we have to cast to the destination's view of the type.
        let llarg = alt fcx.llargs.find(va.id) { some(local_mem(x)) { x } };
        let arg_ty = arg_tys[i].ty;
        if ty::type_contains_params(bcx_tcx(bcx), arg_ty) {
            lldestptr = PointerCast(bcx, lldestptr, val_ty(llarg));
        }
        bcx = memmove_ty(bcx, lldestptr, llarg, arg_ty);
        i += 1u;
    }
    build_return(bcx);
    finish_fn(fcx, lltop);
}


// FIXME: this should do some structural hash-consing to avoid
// duplicate constants. I think. Maybe LLVM has a magical mode
// that does so later on?
fn trans_const_expr(cx: @crate_ctxt, e: @ast::expr) -> ValueRef {
    alt e.node {
      ast::expr_lit(lit) { ret trans_crate_lit(cx, *lit); }
      ast::expr_binary(b, e1, e2) {
        let te1 = trans_const_expr(cx, e1);
        let te2 = trans_const_expr(cx, e2);
        /* Neither type is bottom, and we expect them to be unified already,
         * so the following is safe. */
        let ty = ty::expr_ty(ccx_tcx(cx), e1);
        let is_float = ty::type_is_fp(ccx_tcx(cx), ty);
        let signed = ty::type_is_signed(ccx_tcx(cx), ty);
        ret alt b {
          ast::add.    {
            if is_float { llvm::LLVMConstFAdd(te1, te2) }
            else        { llvm::LLVMConstAdd(te1, te2) }
          }
          ast::subtract. {
            if is_float { llvm::LLVMConstFSub(te1, te2) }
            else        { llvm::LLVMConstSub(te1, te2) }
          }
          ast::mul.    {
            if is_float { llvm::LLVMConstFMul(te1, te2) }
            else        { llvm::LLVMConstMul(te1, te2) }
          }
          ast::div.    {
            if is_float    { llvm::LLVMConstFDiv(te1, te2) }
            else if signed { llvm::LLVMConstSDiv(te1, te2) }
            else           { llvm::LLVMConstUDiv(te1, te2) }
          }
          ast::rem.    {
            if is_float    { llvm::LLVMConstFRem(te1, te2) }
            else if signed { llvm::LLVMConstSRem(te1, te2) }
            else           { llvm::LLVMConstURem(te1, te2) }
          }
          ast::and.    |
          ast::or.     { cx.sess.span_unimpl(e.span, "binop logic"); }
          ast::bitxor. { llvm::LLVMConstXor(te1, te2) }
          ast::bitand. { llvm::LLVMConstAnd(te1, te2) }
          ast::bitor.  { llvm::LLVMConstOr(te1, te2) }
          ast::lsl.    { llvm::LLVMConstShl(te1, te2) }
          ast::lsr.    { llvm::LLVMConstLShr(te1, te2) }
          ast::asr.    { llvm::LLVMConstAShr(te1, te2) }
          ast::eq.     |
          ast::lt.     |
          ast::le.     |
          ast::ne.     |
          ast::ge.     |
          ast::gt.     { cx.sess.span_unimpl(e.span, "binop comparator"); }
        }
      }
      ast::expr_unary(u, e) {
        let te = trans_const_expr(cx, e);
        let ty = ty::expr_ty(ccx_tcx(cx), e);
        let is_float = ty::type_is_fp(ccx_tcx(cx), ty);
        ret alt u {
          ast::box(_)  |
          ast::uniq(_) |
          ast::deref.  { cx.sess.span_bug(e.span,
                           "bad unop type in trans_const_expr"); }
          ast::not.    { llvm::LLVMConstNot(te) }
          ast::neg.    {
            if is_float { llvm::LLVMConstFNeg(te) }
            else        { llvm::LLVMConstNeg(te) }
          }
        }
      }
      _ { cx.sess.span_bug(e.span,
            "bad constant expression type in trans_const_expr"); }
    }
}

fn trans_const(cx: @crate_ctxt, e: @ast::expr, id: ast::node_id) {
    let v = trans_const_expr(cx, e);

    // The scalars come back as 1st class LLVM vals
    // which we have to stick into global constants.

    alt cx.consts.find(id) {
      some(g) {
        llvm::LLVMSetInitializer(g, v);
        llvm::LLVMSetGlobalConstant(g, True);
      }
      _ { cx.sess.span_fatal(e.span, "Unbound const in trans_const"); }
    }
}

type c_stack_tys = {
    arg_tys: [TypeRef],
    ret_ty: TypeRef,
    ret_def: bool,
    base_fn_ty: TypeRef,
    bundle_ty: TypeRef,
    shim_fn_ty: TypeRef
};

fn c_stack_tys(ccx: @crate_ctxt,
               sp: span,
               id: ast::node_id) -> @c_stack_tys {
    alt ty::struct(ccx.tcx, ty::node_id_to_type(ccx.tcx, id)) {
      ty::ty_native_fn(arg_tys, ret_ty) {
        let tcx = ccx.tcx;
        let llargtys = type_of_explicit_args(ccx, sp, arg_tys);
        check non_ty_var(ccx, ret_ty); // NDM does this truly hold?
        let llretty = type_of_inner(ccx, sp, ret_ty);
        let bundle_ty = T_struct(llargtys + [T_ptr(llretty)]);
        ret @{
            arg_tys: llargtys,
            ret_ty: llretty,
            ret_def: !ty::type_is_bot(tcx, ret_ty) &&
                !ty::type_is_nil(tcx, ret_ty),
            base_fn_ty: T_fn(llargtys, llretty),
            bundle_ty: bundle_ty,
            shim_fn_ty: T_fn([T_ptr(bundle_ty)], T_void())
        };
      }

      _ {
        ccx.sess.span_fatal(
            sp,
            "Non-function type for native fn");
      }
    }
}

// For each native function F, we generate a wrapper function W and a shim
// function S that all work together.  The wrapper function W is the function
// that other rust code actually invokes.  Its job is to marshall the
// arguments into a struct.  It then uses a small bit of assembly to switch
// over to the C stack and invoke the shim function.  The shim function S then
// unpacks the arguments from the struct and invokes the actual function F
// according to its specified calling convention.
//
// Example: Given a native c-stack function F(x: X, y: Y) -> Z,
// we generate a wrapper function W that looks like:
//
//    void W(Z* dest, void *env, X x, Y y) {
//        struct { X x; Y y; Z *z; } args = { x, y, z };
//        call_on_c_stack_shim(S, &args);
//    }
//
// The shim function S then looks something like:
//
//     void S(struct { X x; Y y; Z *z; } *args) {
//         *args->z = F(args->x, args->y);
//     }
//
// However, if the return type of F is dynamically sized or of aggregate type,
// the shim function looks like:
//
//     void S(struct { X x; Y y; Z *z; } *args) {
//         F(args->z, args->x, args->y);
//     }
//
// Note: on i386, the layout of the args struct is generally the same as the
// desired layout of the arguments on the C stack.  Therefore, we could use
// upcall_alloc_c_stack() to allocate the `args` structure and switch the
// stack pointer appropriately to avoid a round of copies.  (In fact, the shim
// function itself is unnecessary). We used to do this, in fact, and will
// perhaps do so in the future.
fn trans_native_mod(lcx: @local_ctxt, native_mod: ast::native_mod,
                    abi: ast::native_abi) {
    fn build_shim_fn(lcx: @local_ctxt,
                     native_item: @ast::native_item,
                     tys: @c_stack_tys,
                     cc: uint) -> ValueRef {
        let lname = link_name(native_item);
        let ccx = lcx_ccx(lcx);
        let span = native_item.span;

        // Declare the "prototype" for the base function F:
        let llbasefn = decl_fn(ccx.llmod, lname, cc, tys.base_fn_ty);

        // Create the shim function:
        let shim_name = lname + "__c_stack_shim";
        let llshimfn = decl_internal_cdecl_fn(
            ccx.llmod, shim_name, tys.shim_fn_ty);

        // Declare the body of the shim function:
        let fcx = new_fn_ctxt(lcx, span, llshimfn);
        let bcx = new_top_block_ctxt(fcx);
        let lltop = bcx.llbb;
        let llargbundle = llvm::LLVMGetParam(llshimfn, 0 as c_uint);
        let i = 0u, n = vec::len(tys.arg_tys);
        let llargvals = [];
        while i < n {
            let llargval = load_inbounds(bcx, llargbundle, [0, i as int]);
            llargvals += [llargval];
            i += 1u;
        }

        // Create the call itself and store the return value:
        let llretval = CallWithConv(bcx, llbasefn,
                                    llargvals, cc as c_uint); // r
        if tys.ret_def {
            // R** llretptr = &args->r;
            let llretptr = GEPi(bcx, llargbundle, [0, n as int]);
            // R* llretloc = *llretptr; /* (args->r) */
            let llretloc = Load(bcx, llretptr);
            // *args->r = r;
            Store(bcx, llretval, llretloc);
        }

        // Finish up:
        build_return(bcx);
        finish_fn(fcx, lltop);

        ret llshimfn;
    }

    fn build_wrap_fn(lcx: @local_ctxt,
                     native_item: @ast::native_item,
                     tys: @c_stack_tys,
                     num_tps: uint,
                     llshimfn: ValueRef,
                     llwrapfn: ValueRef) {
        let span = native_item.span;
        let ccx = lcx_ccx(lcx);
        let fcx = new_fn_ctxt(lcx, span, llwrapfn);
        let bcx = new_top_block_ctxt(fcx);
        let lltop = bcx.llbb;

        // Allocate the struct and write the arguments into it.
        let llargbundle = alloca(bcx, tys.bundle_ty);
        let i = 0u, n = vec::len(tys.arg_tys);
        let implicit_args = 2u + num_tps; // ret + env
        while i < n {
            let llargval = llvm::LLVMGetParam(llwrapfn,
                                              (i + implicit_args) as c_uint);
            store_inbounds(bcx, llargval, llargbundle, [0, i as int]);
            i += 1u;
        }
        let llretptr = llvm::LLVMGetParam(llwrapfn, 0 as c_uint);
        store_inbounds(bcx, llretptr, llargbundle, [0, n as int]);

        // Create call itself.
        let call_shim_on_c_stack = ccx.upcalls.call_shim_on_c_stack;
        let llshimfnptr = PointerCast(bcx, llshimfn, T_ptr(T_i8()));
        let llrawargbundle = PointerCast(bcx, llargbundle, T_ptr(T_i8()));
        Call(bcx, call_shim_on_c_stack, [llrawargbundle, llshimfnptr]);
        build_return(bcx);
        finish_fn(fcx, lltop);
    }

    let ccx = lcx_ccx(lcx);
    let cc = lib::llvm::LLVMCCallConv;
    alt abi {
      ast::native_abi_rust_intrinsic. { ret; }
      ast::native_abi_cdecl. { cc = lib::llvm::LLVMCCallConv; }
      ast::native_abi_stdcall. { cc = lib::llvm::LLVMX86StdcallCallConv; }
    }

    for native_item in native_mod.items {
      alt native_item.node {
        ast::native_item_ty. {}
        ast::native_item_fn(fn_decl, tps) {
          let span = native_item.span;
          let id = native_item.id;
          let tys = c_stack_tys(ccx, span, id);
          alt ccx.item_ids.find(id) {
            some(llwrapfn) {
              let llshimfn = build_shim_fn(lcx, native_item, tys, cc);
              build_wrap_fn(lcx, native_item, tys,
                            vec::len(tps), llshimfn, llwrapfn);
            }

            none. {
              ccx.sess.span_fatal(
                  native_item.span,
                  "unbound function item in trans_native_mod");
            }
          }
        }
      }
    }
}

fn trans_item(cx: @local_ctxt, item: ast::item) {
    alt item.node {
      ast::item_fn(decl, tps, body) {
        let sub_cx = extend_path(cx, item.ident);
        alt cx.ccx.item_ids.find(item.id) {
          some(llfndecl) {
            trans_fn(sub_cx, item.span, decl, body, llfndecl, no_self, tps,
                     item.id);
          }
          _ {
            cx.ccx.sess.span_fatal(item.span,
                                   "unbound function item in trans_item");
          }
        }
      }
      ast::item_impl(tps, _, _, ms) {
        trans_impl::trans_impl(cx, item.ident, ms, item.id, tps);
      }
      ast::item_res(decl, tps, body, dtor_id, ctor_id) {
        trans_res_ctor(cx, item.span, decl, ctor_id, tps);

        // Create a function for the destructor
        alt cx.ccx.item_ids.find(item.id) {
          some(lldtor_decl) {
            trans_fn(cx, item.span, decl, body, lldtor_decl, no_self,
                     tps, dtor_id);
          }
          _ {
            cx.ccx.sess.span_fatal(item.span, "unbound dtor in trans_item");
          }
        }
      }
      ast::item_mod(m) {
        let sub_cx =
            @{path: cx.path + [item.ident],
              module_path: cx.module_path + [item.ident] with *cx};
        trans_mod(sub_cx, m);
      }
      ast::item_tag(variants, tps) {
        let sub_cx = extend_path(cx, item.ident);
        let degen = vec::len(variants) == 1u;
        let vi = ty::tag_variants(cx.ccx.tcx, {crate: ast::local_crate,
                                               node: item.id});
        let i = 0;
        for variant: ast::variant in variants {
            trans_tag_variant(sub_cx, item.id, variant,
                              vi[i].disr_val, degen, tps);
            i += 1;
        }
      }
      ast::item_const(_, expr) { trans_const(cx.ccx, expr, item.id); }
      ast::item_native_mod(native_mod) {
        let abi = alt attr::native_abi(item.attrs) {
          either::right(abi_) { abi_ }
          either::left(msg) { cx.ccx.sess.span_fatal(item.span, msg) }
        };
        trans_native_mod(cx, native_mod, abi);
      }
      _ {/* fall through */ }
    }
}

// Translate a module.  Doing this amounts to translating the items in the
// module; there ends up being no artifact (aside from linkage names) of
// separate modules in the compiled program.  That's because modules exist
// only as a convenience for humans working with the code, to organize names
// and control visibility.
fn trans_mod(cx: @local_ctxt, m: ast::_mod) {
    for item: @ast::item in m.items { trans_item(cx, *item); }
}

fn get_pair_fn_ty(llpairty: TypeRef) -> TypeRef {
    // Bit of a kludge: pick the fn typeref out of the pair.

    ret struct_elt(llpairty, 0u);
}

fn register_fn(ccx: @crate_ctxt, sp: span, path: [str], flav: str,
               ty_params: [ast::ty_param], node_id: ast::node_id) {
    // FIXME: pull this out
    let t = node_id_type(ccx, node_id);
    check returns_non_ty_var(ccx, t);
    register_fn_full(ccx, sp, path, flav, ty_params, node_id, t);
}

fn param_bounds(ccx: @crate_ctxt, tp: ast::ty_param) -> ty::param_bounds {
    ccx.tcx.ty_param_bounds.get(tp.id)
}

fn register_fn_full(ccx: @crate_ctxt, sp: span, path: [str], _flav: str,
                    tps: [ast::ty_param], node_id: ast::node_id,
                    node_type: ty::t)
    : returns_non_ty_var(ccx, node_type) {
    let path = path;
    let llfty = type_of_fn_from_ty(ccx, sp, node_type,
                                   vec::map(tps, {|p| param_bounds(ccx, p)}));
    let ps: str = mangle_exported_name(ccx, path, node_type);
    let llfn: ValueRef = decl_cdecl_fn(ccx.llmod, ps, llfty);
    ccx.item_ids.insert(node_id, llfn);
    ccx.item_symbols.insert(node_id, ps);

    let is_main: bool = is_main_name(path) && !ccx.sess.building_library;
    if is_main { create_main_wrapper(ccx, sp, llfn, node_type); }
}

// Create a _rust_main(args: [str]) function which will be called from the
// runtime rust_start function
fn create_main_wrapper(ccx: @crate_ctxt, sp: span, main_llfn: ValueRef,
                       main_node_type: ty::t) {

    if ccx.main_fn != none::<ValueRef> {
        ccx.sess.span_fatal(sp, "multiple 'main' functions");
    }

    let main_takes_argv =
        alt ty::struct(ccx.tcx, main_node_type) {
          ty::ty_fn({inputs, _}) { vec::len(inputs) != 0u }
        };

    let llfn = create_main(ccx, sp, main_llfn, main_takes_argv);
    ccx.main_fn = some(llfn);
    create_entry_fn(ccx, llfn);

    fn create_main(ccx: @crate_ctxt, sp: span, main_llfn: ValueRef,
                   takes_argv: bool) -> ValueRef {
        let unit_ty = ty::mk_str(ccx.tcx);
        let vecarg_ty: ty::arg =
            {mode: ast::by_val,
             ty: ty::mk_vec(ccx.tcx, {ty: unit_ty, mut: ast::imm})};
        // FIXME: mk_nil should have a postcondition
        let nt = ty::mk_nil(ccx.tcx);
        let llfty = type_of_fn(ccx, sp, [vecarg_ty], nt, []);
        let llfdecl = decl_fn(ccx.llmod, "_rust_main",
                              lib::llvm::LLVMCCallConv, llfty);

        let fcx = new_fn_ctxt(new_local_ctxt(ccx), sp, llfdecl);

        let bcx = new_top_block_ctxt(fcx);
        let lltop = bcx.llbb;

        let lloutputarg = llvm::LLVMGetParam(llfdecl, 0 as c_uint);
        let llenvarg = llvm::LLVMGetParam(llfdecl, 1 as c_uint);
        let args = [lloutputarg, llenvarg];
        if takes_argv { args += [llvm::LLVMGetParam(llfdecl, 2 as c_uint)]; }
        Call(bcx, main_llfn, args);
        build_return(bcx);

        finish_fn(fcx, lltop);

        ret llfdecl;
    }

    fn create_entry_fn(ccx: @crate_ctxt, rust_main: ValueRef) {
        #[cfg(target_os = "win32")]
        fn main_name() -> str { ret "WinMain@16"; }
        #[cfg(target_os = "macos")]
        fn main_name() -> str { ret "main"; }
        #[cfg(target_os = "linux")]
        fn main_name() -> str { ret "main"; }
        #[cfg(target_os = "freebsd")]
        fn main_name() -> str { ret "main"; }
        let llfty = T_fn([ccx.int_type, ccx.int_type], ccx.int_type);
        let llfn = decl_cdecl_fn(ccx.llmod, main_name(), llfty);
        let llbb = str::as_buf("top", {|buf|
            llvm::LLVMAppendBasicBlock(llfn, buf)
        });
        let bld = *ccx.builder;
        llvm::LLVMPositionBuilderAtEnd(bld, llbb);
        let crate_map = ccx.crate_map;
        let start_ty = T_fn([val_ty(rust_main), ccx.int_type, ccx.int_type,
                             val_ty(crate_map)], ccx.int_type);
        let start = str::as_buf("rust_start", {|buf|
            llvm::LLVMAddGlobal(ccx.llmod, start_ty, buf)
        });
        let args = [rust_main, llvm::LLVMGetParam(llfn, 0 as c_uint),
                    llvm::LLVMGetParam(llfn, 1 as c_uint), crate_map];
        let result = unsafe {
            llvm::LLVMBuildCall(bld, start, vec::to_ptr(args),
                                vec::len(args) as c_uint, noname())
        };
        llvm::LLVMBuildRet(bld, result);
    }
}

// Create a /real/ closure: this is like create_fn_pair, but creates a
// a fn value on the stack with a specified environment (which need not be
// on the stack).
fn create_real_fn_pair(cx: @block_ctxt, llfnty: TypeRef, llfn: ValueRef,
                       llenvptr: ValueRef) -> ValueRef {
    let lcx = cx.fcx.lcx;

    let pair = alloca(cx, T_fn_pair(lcx.ccx, llfnty));
    fill_fn_pair(cx, pair, llfn, llenvptr);
    ret pair;
}

fn fill_fn_pair(bcx: @block_ctxt, pair: ValueRef, llfn: ValueRef,
                llenvptr: ValueRef) {
    let ccx = bcx_ccx(bcx);
    let code_cell = GEPi(bcx, pair, [0, abi::fn_field_code]);
    Store(bcx, llfn, code_cell);
    let env_cell = GEPi(bcx, pair, [0, abi::fn_field_box]);
    let llenvblobptr =
        PointerCast(bcx, llenvptr, T_opaque_cbox_ptr(ccx));
    Store(bcx, llenvblobptr, env_cell);
}

// Returns the number of type parameters that the given native function has.
fn native_fn_ty_param_count(cx: @crate_ctxt, id: ast::node_id) -> uint {
    let count;
    let native_item =
        alt cx.ast_map.find(id) { some(ast_map::node_native_item(i)) { i } };
    alt native_item.node {
      ast::native_item_ty. {
        cx.sess.bug("register_native_fn(): native fn isn't \
                        actually a fn");
      }
      ast::native_item_fn(_, tps) {
        count = vec::len::<ast::ty_param>(tps);
      }
    }
    ret count;
}

fn native_fn_wrapper_type(cx: @crate_ctxt, sp: span,
                          param_bounds: [ty::param_bounds],
                          x: ty::t) -> TypeRef {
    alt ty::struct(cx.tcx, x) {
      ty::ty_native_fn(args, out) {
        ret type_of_fn(cx, sp, args, out, param_bounds);
      }
    }
}

fn raw_native_fn_type(ccx: @crate_ctxt, sp: span, args: [ty::arg],
                      ret_ty: ty::t) -> TypeRef {
    check type_has_static_size(ccx, ret_ty);
    ret T_fn(type_of_explicit_args(ccx, sp, args), type_of(ccx, sp, ret_ty));
}

fn link_name(i: @ast::native_item) -> str {
    alt attr::get_meta_item_value_str_by_name(i.attrs, "link_name") {
      none. { ret i.ident; }
      option::some(ln) { ret ln; }
    }
}

fn collect_native_item(ccx: @crate_ctxt,
                       abi: @mutable option::t<ast::native_abi>,
                       i: @ast::native_item,
                       &&pt: [str],
                       _v: vt<[str]>) {
    alt i.node {
      ast::native_item_fn(_, tps) {
        let sp = i.span;
        let id = i.id;
        let node_type = node_id_type(ccx, id);
        let fn_abi =
            alt attr::get_meta_item_value_str_by_name(i.attrs, "abi") {
            option::none. {
                // if abi isn't specified for this function, inherit from
                  // its enclosing native module
                  option::get(*abi)
              }
                _ {
                    alt attr::native_abi(i.attrs) {
                      either::right(abi_) { abi_ }
                      either::left(msg) { ccx.sess.span_fatal(i.span, msg) }
                    }
                }
            };
        alt fn_abi {
          ast::native_abi_rust_intrinsic. {
            // For intrinsics: link the function directly to the intrinsic
            // function itself.
            let fn_type = type_of_fn_from_ty(
                ccx, sp, node_type,
                vec::map(tps, {|p| param_bounds(ccx, p)}));
            let ri_name = "rust_intrinsic_" + link_name(i);
            let llnativefn = get_extern_fn(
                ccx.externs, ccx.llmod, ri_name,
                lib::llvm::LLVMCCallConv, fn_type);
            ccx.item_ids.insert(id, llnativefn);
            ccx.item_symbols.insert(id, ri_name);
          }

          ast::native_abi_cdecl. | ast::native_abi_stdcall. {
            // For true external functions: create a rust wrapper
            // and link to that.  The rust wrapper will handle
            // switching to the C stack.
            let new_pt = pt + [i.ident];
            register_fn(ccx, i.span, new_pt, "native fn", tps, i.id);
          }
        }
      }
      _ { }
    }
}

fn collect_item(ccx: @crate_ctxt, abi: @mutable option::t<ast::native_abi>,
                i: @ast::item, &&pt: [str], v: vt<[str]>) {
    let new_pt = pt + [i.ident];
    alt i.node {
      ast::item_const(_, _) {
        let typ = node_id_type(ccx, i.id);
        let s =
            mangle_exported_name(ccx, pt + [i.ident],
                                 node_id_type(ccx, i.id));
        // FIXME: Could follow from a constraint on types of const
        // items
        let g = str::as_buf(s, {|buf|
            check (type_has_static_size(ccx, typ));
            llvm::LLVMAddGlobal(ccx.llmod, type_of(ccx, i.span, typ), buf)
        });
        ccx.item_symbols.insert(i.id, s);
        ccx.consts.insert(i.id, g);
      }
      ast::item_native_mod(native_mod) {
        // Propagate the native ABI down to collect_native_item(),
        alt attr::native_abi(i.attrs) {
          either::left(msg) { ccx.sess.span_fatal(i.span, msg); }
          either::right(abi_) {
            *abi = option::some(abi_);
          }
        }
      }
      ast::item_fn(_, tps, _) {
        register_fn(ccx, i.span, new_pt, "fn", tps, i.id);
      }
      ast::item_impl(tps, _, _, methods) {
        let name = i.ident + int::str(i.id);
        for m in methods {
            register_fn(ccx, i.span, pt + [name, m.ident],
                        "impl_method", tps + m.tps, m.id);
        }
      }
      ast::item_res(_, tps, _, dtor_id, ctor_id) {
        register_fn(ccx, i.span, new_pt, "res_ctor", tps, ctor_id);
        // Note that the destructor is associated with the item's id, not
        // the dtor_id. This is a bit counter-intuitive, but simplifies
        // ty_res, which would have to carry around two def_ids otherwise
        // -- one to identify the type, and one to find the dtor symbol.
        let t = node_id_type(ccx, dtor_id);
        // FIXME: how to get rid of this check?
        check returns_non_ty_var(ccx, t);
        register_fn_full(ccx, i.span, new_pt, "res_dtor", tps, i.id, t);
      }
      ast::item_tag(variants, tps) {
        for variant in variants {
            if vec::len(variant.node.args) != 0u {
                register_fn(ccx, i.span, new_pt + [variant.node.name],
                            "tag", tps, variant.node.id);
            }
        }
      }
      _ { }
    }
    visit::visit_item(i, new_pt, v);
}

fn collect_items(ccx: @crate_ctxt, crate: @ast::crate) {
    let abi = @mutable none::<ast::native_abi>;
    visit::visit_crate(*crate, [], visit::mk_vt(@{
        visit_native_item: bind collect_native_item(ccx, abi, _, _, _),
        visit_item: bind collect_item(ccx, abi, _, _, _)
        with *visit::default_visitor()
    }));
}

// The constant translation pass.
fn trans_constant(ccx: @crate_ctxt, it: @ast::item, &&pt: [str],
                  v: vt<[str]>) {
    let new_pt = pt + [it.ident];
    visit::visit_item(it, new_pt, v);
    alt it.node {
      ast::item_tag(variants, _) {
        let vi = ty::tag_variants(ccx.tcx, {crate: ast::local_crate,
                                            node: it.id});
        let i = 0;
        for variant in variants {
            let p = new_pt + [variant.node.name, "discrim"];
            let s = mangle_exported_name(ccx, p, ty::mk_int(ccx.tcx));
            let disr_val = vi[i].disr_val;
            let discrim_gvar = str::as_buf(s, {|buf|
                llvm::LLVMAddGlobal(ccx.llmod, ccx.int_type, buf)
            });
            llvm::LLVMSetInitializer(discrim_gvar, C_int(ccx, disr_val));
            llvm::LLVMSetGlobalConstant(discrim_gvar, True);
            ccx.discrims.insert(
                ast_util::local_def(variant.node.id), discrim_gvar);
            ccx.discrim_symbols.insert(variant.node.id, s);
            i += 1;
        }
      }
      ast::item_impl(tps, some(@{node: ast::ty_path(_, id), _}), _, ms) {
        let i_did = ast_util::def_id_of_def(ccx.tcx.def_map.get(id));
        trans_impl::trans_impl_vtable(ccx, pt, i_did, ms, tps, it);
      }
      ast::item_iface(_, _) {
        trans_impl::trans_iface_vtable(ccx, pt, it);
      }
      _ { }
    }
}

fn trans_constants(ccx: @crate_ctxt, crate: @ast::crate) {
    let visitor =
        @{visit_item: bind trans_constant(ccx, _, _, _)
             with *visit::default_visitor()};
    visit::visit_crate(*crate, [], visit::mk_vt(visitor));
}

fn vp2i(cx: @block_ctxt, v: ValueRef) -> ValueRef {
    let ccx = bcx_ccx(cx);
    ret PtrToInt(cx, v, ccx.int_type);
}

fn p2i(ccx: @crate_ctxt, v: ValueRef) -> ValueRef {
    ret llvm::LLVMConstPtrToInt(v, ccx.int_type);
}

fn declare_intrinsics(llmod: ModuleRef) -> hashmap<str, ValueRef> {
    let T_memmove32_args: [TypeRef] =
        [T_ptr(T_i8()), T_ptr(T_i8()), T_i32(), T_i32(), T_i1()];
    let T_memmove64_args: [TypeRef] =
        [T_ptr(T_i8()), T_ptr(T_i8()), T_i64(), T_i32(), T_i1()];
    let T_memset32_args: [TypeRef] =
        [T_ptr(T_i8()), T_i8(), T_i32(), T_i32(), T_i1()];
    let T_memset64_args: [TypeRef] =
        [T_ptr(T_i8()), T_i8(), T_i64(), T_i32(), T_i1()];
    let T_trap_args: [TypeRef] = [];
    let gcroot =
        decl_cdecl_fn(llmod, "llvm.gcroot",
                      T_fn([T_ptr(T_ptr(T_i8())), T_ptr(T_i8())], T_void()));
    let gcread =
        decl_cdecl_fn(llmod, "llvm.gcread",
                      T_fn([T_ptr(T_i8()), T_ptr(T_ptr(T_i8()))], T_void()));
    let memmove32 =
        decl_cdecl_fn(llmod, "llvm.memmove.p0i8.p0i8.i32",
                      T_fn(T_memmove32_args, T_void()));
    let memmove64 =
        decl_cdecl_fn(llmod, "llvm.memmove.p0i8.p0i8.i64",
                      T_fn(T_memmove64_args, T_void()));
    let memset32 =
        decl_cdecl_fn(llmod, "llvm.memset.p0i8.i32",
                      T_fn(T_memset32_args, T_void()));
    let memset64 =
        decl_cdecl_fn(llmod, "llvm.memset.p0i8.i64",
                      T_fn(T_memset64_args, T_void()));
    let trap = decl_cdecl_fn(llmod, "llvm.trap", T_fn(T_trap_args, T_void()));
    let intrinsics = new_str_hash::<ValueRef>();
    intrinsics.insert("llvm.gcroot", gcroot);
    intrinsics.insert("llvm.gcread", gcread);
    intrinsics.insert("llvm.memmove.p0i8.p0i8.i32", memmove32);
    intrinsics.insert("llvm.memmove.p0i8.p0i8.i64", memmove64);
    intrinsics.insert("llvm.memset.p0i8.i32", memset32);
    intrinsics.insert("llvm.memset.p0i8.i64", memset64);
    intrinsics.insert("llvm.trap", trap);
    ret intrinsics;
}

fn declare_dbg_intrinsics(llmod: ModuleRef,
                          intrinsics: hashmap<str, ValueRef>) {
    let declare =
        decl_cdecl_fn(llmod, "llvm.dbg.declare",
                      T_fn([T_metadata(), T_metadata()], T_void()));
    let value =
        decl_cdecl_fn(llmod, "llvm.dbg.value",
                      T_fn([T_metadata(), T_i64(), T_metadata()], T_void()));
    intrinsics.insert("llvm.dbg.declare", declare);
    intrinsics.insert("llvm.dbg.value", value);
}

fn trap(bcx: @block_ctxt) {
    let v: [ValueRef] = [];
    alt bcx_ccx(bcx).intrinsics.find("llvm.trap") {
      some(x) { Call(bcx, x, v); }
      _ { bcx_ccx(bcx).sess.bug("unbound llvm.trap in trap"); }
    }
}

fn create_module_map(ccx: @crate_ctxt) -> ValueRef {
    let elttype = T_struct([ccx.int_type, ccx.int_type]);
    let maptype = T_array(elttype, ccx.module_data.size() + 1u);
    let map =
        str::as_buf("_rust_mod_map",
                    {|buf| llvm::LLVMAddGlobal(ccx.llmod, maptype, buf) });
    llvm::LLVMSetLinkage(map,
                         lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    let elts: [ValueRef] = [];
    ccx.module_data.items {|key, val|
        let elt = C_struct([p2i(ccx, C_cstr(ccx, key)),
                            p2i(ccx, val)]);
        elts += [elt];
    };
    let term = C_struct([C_int(ccx, 0), C_int(ccx, 0)]);
    elts += [term];
    llvm::LLVMSetInitializer(map, C_array(elttype, elts));
    ret map;
}


fn decl_crate_map(sess: session::session, mapname: str,
                  llmod: ModuleRef) -> ValueRef {
    let targ_cfg = sess.targ_cfg;
    let int_type = T_int(targ_cfg);
    let n_subcrates = 1;
    let cstore = sess.cstore;
    while cstore::have_crate_data(cstore, n_subcrates) { n_subcrates += 1; }
    let mapname = sess.building_library ? mapname : "toplevel";
    let sym_name = "_rust_crate_map_" + mapname;
    let arrtype = T_array(int_type, n_subcrates as uint);
    let maptype = T_struct([int_type, arrtype]);
    let map = str::as_buf(sym_name, {|buf|
        llvm::LLVMAddGlobal(llmod, maptype, buf)
    });
    llvm::LLVMSetLinkage(map, lib::llvm::LLVMExternalLinkage
                         as llvm::Linkage);
    ret map;
}

// FIXME use hashed metadata instead of crate names once we have that
fn fill_crate_map(ccx: @crate_ctxt, map: ValueRef) {
    let subcrates: [ValueRef] = [];
    let i = 1;
    let cstore = ccx.sess.cstore;
    while cstore::have_crate_data(cstore, i) {
        let nm = "_rust_crate_map_" + cstore::get_crate_data(cstore, i).name;
        let cr = str::as_buf(nm, {|buf|
            llvm::LLVMAddGlobal(ccx.llmod, ccx.int_type, buf)
        });
        subcrates += [p2i(ccx, cr)];
        i += 1;
    }
    subcrates += [C_int(ccx, 0)];
    llvm::LLVMSetInitializer(map, C_struct(
        [p2i(ccx, create_module_map(ccx)),
         C_array(ccx.int_type, subcrates)]));
}

fn write_metadata(cx: @crate_ctxt, crate: @ast::crate) {
    if !cx.sess.building_library { ret; }
    let llmeta = C_postr(metadata::encoder::encode_metadata(cx, crate));
    let llconst = trans_common::C_struct([llmeta]);
    let llglobal =
        str::as_buf("rust_metadata",
                    {|buf|
                        llvm::LLVMAddGlobal(cx.llmod, val_ty(llconst), buf)
                    });
    llvm::LLVMSetInitializer(llglobal, llconst);
    let _: () =
        str::as_buf(cx.sess.targ_cfg.target_strs.meta_sect_name,
                    {|buf| llvm::LLVMSetSection(llglobal, buf) });
    llvm::LLVMSetLinkage(llglobal,
                         lib::llvm::LLVMInternalLinkage as llvm::Linkage);

    let t_ptr_i8 = T_ptr(T_i8());
    llglobal = llvm::LLVMConstBitCast(llglobal, t_ptr_i8);
    let llvm_used =
        str::as_buf("llvm.used",
                    {|buf|
                        llvm::LLVMAddGlobal(cx.llmod, T_array(t_ptr_i8, 1u),
                                            buf)
                    });
    llvm::LLVMSetLinkage(llvm_used,
                         lib::llvm::LLVMAppendingLinkage as llvm::Linkage);
    llvm::LLVMSetInitializer(llvm_used, C_array(t_ptr_i8, [llglobal]));
}

// Writes the current ABI version into the crate.
fn write_abi_version(ccx: @crate_ctxt) {
    shape::mk_global(ccx, "rust_abi_version", C_uint(ccx, abi::abi_version),
                     false);
}

fn trans_crate(sess: session::session, crate: @ast::crate, tcx: ty::ctxt,
               output: str, emap: resolve::exp_map, amap: ast_map::map,
               mut_map: mut::mut_map, copy_map: alias::copy_map,
               last_uses: last_use::last_uses, method_map: typeck::method_map,
               dict_map: typeck::dict_map)
    -> (ModuleRef, link::link_meta) {
    let sha = std::sha1::mk_sha1();
    let link_meta = link::build_link_meta(sess, *crate, output, sha);

    // Append ".rc" to crate name as LLVM module identifier.
    //
    // LLVM code generator emits a ".file filename" directive
    // for ELF backends. Value of the "filename" is set as the
    // LLVM module identifier.  Due to a LLVM MC bug[1], LLVM
    // crashes if the module identifer is same as other symbols
    // such as a function name in the module.
    // 1. http://llvm.org/bugs/show_bug.cgi?id=11479
    let llmod_id = link_meta.name + ".rc";

    let llmod = str::as_buf(llmod_id, {|buf|
        llvm::LLVMModuleCreateWithNameInContext
            (buf, llvm::LLVMGetGlobalContext())
    });
    let data_layout = sess.targ_cfg.target_strs.data_layout;
    let targ_triple = sess.targ_cfg.target_strs.target_triple;
    let _: () =
        str::as_buf(data_layout,
                    {|buf| llvm::LLVMSetDataLayout(llmod, buf) });
    let _: () =
        str::as_buf(targ_triple,
                    {|buf| llvm::LLVMSetTarget(llmod, buf) });
    let targ_cfg = sess.targ_cfg;
    let td = mk_target_data(sess.targ_cfg.target_strs.data_layout);
    let tn = mk_type_names();
    let intrinsics = declare_intrinsics(llmod);
    if sess.opts.extra_debuginfo {
        declare_dbg_intrinsics(llmod, intrinsics);
    }
    let int_type = T_int(targ_cfg);
    let float_type = T_float(targ_cfg);
    let task_type = T_task(targ_cfg);
    let taskptr_type = T_ptr(task_type);
    lib::llvm::associate_type(tn, "taskptr", taskptr_type);
    let tydesc_type = T_tydesc(targ_cfg);
    lib::llvm::associate_type(tn, "tydesc", tydesc_type);
    let crate_map = decl_crate_map(sess, link_meta.name, llmod);
    let dbg_cx = if sess.opts.debuginfo {
        option::some(@{llmetadata: map::new_int_hash(),
                       names: new_namegen()})
    } else {
        option::none
    };
    let ccx =
        @{sess: sess,
          llmod: llmod,
          td: td,
          tn: tn,
          externs: new_str_hash::<ValueRef>(),
          intrinsics: intrinsics,
          item_ids: new_int_hash::<ValueRef>(),
          ast_map: amap,
          exp_map: emap,
          item_symbols: new_int_hash::<str>(),
          mutable main_fn: none::<ValueRef>,
          link_meta: link_meta,
          tag_sizes: ty::new_ty_hash(),
          discrims: ast_util::new_def_id_hash::<ValueRef>(),
          discrim_symbols: new_int_hash::<str>(),
          consts: new_int_hash::<ValueRef>(),
          tydescs: ty::new_ty_hash(),
          dicts: map::mk_hashmap(hash_dict_id, {|a, b| a == b}),
          module_data: new_str_hash::<ValueRef>(),
          lltypes: ty::new_ty_hash(),
          names: new_namegen(),
          sha: sha,
          type_sha1s: ty::new_ty_hash(),
          type_short_names: ty::new_ty_hash(),
          tcx: tcx,
          mut_map: mut_map,
          copy_map: copy_map,
          last_uses: last_uses,
          method_map: method_map,
          dict_map: dict_map,
          stats:
              {mutable n_static_tydescs: 0u,
               mutable n_derived_tydescs: 0u,
               mutable n_glues_created: 0u,
               mutable n_null_glues: 0u,
               mutable n_real_glues: 0u,
               fn_times: @mutable []},
          upcalls:
              upcall::declare_upcalls(targ_cfg, tn, tydesc_type,
                                      llmod),
          tydesc_type: tydesc_type,
          int_type: int_type,
          float_type: float_type,
          task_type: task_type,
          opaque_vec_type: T_opaque_vec(targ_cfg),
          builder: BuilderRef_res(llvm::LLVMCreateBuilder()),
          shape_cx: shape::mk_ctxt(llmod),
          gc_cx: gc::mk_ctxt(),
          crate_map: crate_map,
          dbg_cx: dbg_cx};
    let cx = new_local_ctxt(ccx);
    collect_items(ccx, crate);
    trans_constants(ccx, crate);
    trans_mod(cx, crate.node.module);
    fill_crate_map(ccx, crate_map);
    emit_tydescs(ccx);
    shape::gen_shape_tables(ccx);
    write_abi_version(ccx);

    // Translate the metadata.
    write_metadata(cx.ccx, crate);
    if ccx.sess.opts.stats {
        #error("--- trans stats ---");
        #error("n_static_tydescs: %u", ccx.stats.n_static_tydescs);
        #error("n_derived_tydescs: %u", ccx.stats.n_derived_tydescs);
        #error("n_glues_created: %u", ccx.stats.n_glues_created);
        #error("n_null_glues: %u", ccx.stats.n_null_glues);
        #error("n_real_glues: %u", ccx.stats.n_real_glues);

        for timing: {ident: str, time: int} in *ccx.stats.fn_times {
            #error("time: %s took %d ms", timing.ident, timing.time);
        }
    }
    ret (llmod, link_meta);
}
//
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// End:
//
